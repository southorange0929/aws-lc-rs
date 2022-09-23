// Copyright 2015-2016 Brian Smith.
//
// Permission to use, copy, modify, and/or distribute this software for any
// purpose with or without fee is hereby granted, provided that the above
// copyright notice and this permission notice appear in all copies.
//
// THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHORS DISCLAIM ALL WARRANTIES
// WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
// MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHORS BE LIABLE FOR ANY
// SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
// WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN ACTION
// OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF OR IN
// CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.

use crate::error::{KeyRejected, Unspecified};

use crate::ptr::{DetachableLcPtr, IntoPointer, LcPtr, NonNullPtr};

use crate::rsa::evp_pkey;
use crate::signature::{Signature, VerificationAlgorithm};
use crate::{digest, sealed};
use aws_lc_sys::{
    BN_bin2bn, BN_bn2bin, BN_num_bytes, ECDSA_SIG_from_bytes, ECDSA_SIG_new, ECDSA_SIG_set0,
    ECDSA_SIG_to_bytes, ECDSA_do_verify, EC_KEY_get0_group, EC_KEY_get0_public_key,
    EC_KEY_set_private_key, EC_KEY_set_public_key, EC_POINT_new, BIGNUM, ECDSA_SIG, EC_GROUP,
    EC_KEY, EC_POINT, EVP_PKEY,
};
use std::fmt::{Debug, Formatter};
use std::mem::MaybeUninit;
use std::ops::Deref;
use std::os::raw::{c_int, c_uint};
use std::ptr::null_mut;
use std::slice;

pub(crate) mod key_pair;

const ELEM_MAX_BITS: usize = 384;
pub const ELEM_MAX_BYTES: usize = (ELEM_MAX_BITS + 7) / 8;

pub const SCALAR_MAX_BYTES: usize = ELEM_MAX_BYTES;

/// The maximum length, in bytes, of an encoded public key.
const PUBLIC_KEY_MAX_LEN: usize = 1 + (2 * ELEM_MAX_BYTES);

/// The maximum length of a PKCS#8 documents generated by *ring* for ECC keys.
///
/// This is NOT the maximum length of a PKCS#8 document that can be consumed by
/// `pkcs8::unwrap_key()`.
///
/// `40` is the length of the P-384 template. It is actually one byte shorter
/// than the P-256 template, but the private key and the public key are much
/// longer.
pub const PKCS8_DOCUMENT_MAX_LEN: usize = 40 + SCALAR_MAX_BYTES + PUBLIC_KEY_MAX_LEN;

#[derive(Debug)]
pub struct EcdsaVerificationAlgorithm {
    pub(super) id: &'static AlgorithmID,
    pub(super) digest: &'static digest::Algorithm,
    pub(super) bits: c_uint,
    pub(super) nid: i32,
    pub(super) sig_format: EcdsaSignatureFormat,
}

#[derive(Debug)]
pub struct EcdsaSigningAlgorithm(&'static EcdsaVerificationAlgorithm);

impl EcdsaSigningAlgorithm {
    pub const fn new(algorithm: &'static EcdsaVerificationAlgorithm) -> Self {
        EcdsaSigningAlgorithm(algorithm)
    }
}

impl Deref for EcdsaSigningAlgorithm {
    type Target = EcdsaVerificationAlgorithm;

    fn deref(&self) -> &Self::Target {
        self.0
    }
}

impl sealed::Sealed for EcdsaVerificationAlgorithm {}
impl sealed::Sealed for EcdsaSigningAlgorithm {}

#[derive(Debug)]
pub(crate) enum EcdsaSignatureFormat {
    ASN1,
    Fixed,
}

#[derive(Debug, Eq, PartialEq)]
#[allow(non_camel_case_types)]
pub(crate) enum AlgorithmID {
    ECDSA_P256,
    ECDSA_P384,
}

#[derive(Clone)]
pub struct EcdsaPublicKey(Box<[u8]>);

impl Debug for EcdsaPublicKey {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(&format!("PublicKey(\"{}\")", hex::encode(self.0.as_ref())))
    }
}

impl EcdsaPublicKey {
    fn new(pubkey_box: Box<[u8]>) -> Self {
        EcdsaPublicKey(pubkey_box)
    }
}

impl AsRef<[u8]> for EcdsaPublicKey {
    fn as_ref(&self) -> &[u8] {
        self.0.as_ref()
    }
}

unsafe impl Send for EcdsaPublicKey {}
unsafe impl Sync for EcdsaPublicKey {}

impl VerificationAlgorithm for EcdsaVerificationAlgorithm {
    fn verify(&self, public_key: &[u8], msg: &[u8], signature: &[u8]) -> Result<(), Unspecified> {
        unsafe {
            let ec_group = EC_GROUP_from_nid(self.nid)?;
            let ec_point = EC_POINT_from_bytes(&ec_group, public_key)?;
            let ec_key = EC_KEY_from_public_point(&ec_group, &ec_point)?;

            let ecdsa_sig = match self.sig_format {
                EcdsaSignatureFormat::ASN1 => ECDSA_SIG_from_asn1(signature),
                EcdsaSignatureFormat::Fixed => ECDSA_SIG_from_fixed(self.id, signature),
            }?;
            let msg_digest = digest::digest(self.digest, msg);
            let msg_digest = msg_digest.as_ref();

            if 1 != ECDSA_do_verify(msg_digest.as_ptr(), msg_digest.len(), *ecdsa_sig, *ec_key) {
                return Err(Unspecified);
            }

            Ok(())
        }
    }
}

#[inline]
unsafe fn validate_pkey(
    evp_pkey: NonNullPtr<*mut EVP_PKEY>,
    expected_bits: c_uint,
) -> Result<(), KeyRejected> {
    const EC_KEY_TYPE: c_int = aws_lc_sys::EVP_PKEY_EC;
    evp_pkey::validate_pkey(evp_pkey, EC_KEY_TYPE, expected_bits, expected_bits)
}

#[inline]
unsafe fn validate_ec_key(_ec_key: *mut EC_KEY) -> Result<(), KeyRejected> {
    Ok(())
}

fn marshal_public_key(ec_key: &LcPtr<*mut EC_KEY>) -> Result<EcdsaPublicKey, Unspecified> {
    unsafe {
        let ec_group = EC_KEY_get0_group(**ec_key)
            .into_pointer()
            .ok_or(Unspecified)?;

        let ec_point = EC_KEY_get0_public_key(**ec_key)
            .into_pointer()
            .ok_or(Unspecified)?;

        let mut pub_key_bytes = [0u8; PUBLIC_KEY_MAX_LEN];
        let out_len = EC_POINT_to_bytes(ec_group, ec_point, &mut pub_key_bytes)
            .expect("Unexpected: Unable to marshal EC public key ");
        let mut pubkey_vec = Vec::<u8>::new();
        pubkey_vec.extend_from_slice(&pub_key_bytes[0..out_len]);
        Ok(EcdsaPublicKey::new(pubkey_vec.into_boxed_slice()))
    }
}

#[inline]
#[allow(non_snake_case)]
unsafe fn EC_KEY_from_public_point(
    ec_group: &LcPtr<*mut EC_GROUP>,
    public_ec_point: &LcPtr<*mut EC_POINT>,
) -> Result<LcPtr<*mut EC_KEY>, Unspecified> {
    let ec_key = LcPtr::new(aws_lc_sys::EC_KEY_new()).map_err(|_| Unspecified)?;
    if 1 != aws_lc_sys::EC_KEY_set_group(*ec_key, **ec_group) {
        return Err(Unspecified);
    }
    if 1 != EC_KEY_set_public_key(*ec_key, **public_ec_point) {
        return Err(Unspecified);
    }
    Ok(ec_key)
}

#[inline]
#[allow(non_snake_case)]
unsafe fn EC_KEY_from_public_private(
    ec_group: &LcPtr<*mut EC_GROUP>,
    public_ec_point: &LcPtr<*mut EC_POINT>,
    private_bignum: &LcPtr<*mut BIGNUM>,
) -> Result<LcPtr<*mut EC_KEY>, Unspecified> {
    let ec_key = LcPtr::new(aws_lc_sys::EC_KEY_new()).map_err(|_| Unspecified)?;
    if 1 != aws_lc_sys::EC_KEY_set_group(*ec_key, **ec_group) {
        return Err(Unspecified);
    }
    if 1 != EC_KEY_set_public_key(*ec_key, **public_ec_point) {
        return Err(Unspecified);
    }
    if 1 != EC_KEY_set_private_key(*ec_key, **private_bignum) {
        return Err(Unspecified);
    }
    Ok(ec_key)
}

#[inline]
#[allow(non_snake_case)]
unsafe fn EC_GROUP_from_nid(nid: i32) -> Result<LcPtr<*mut EC_GROUP>, Unspecified> {
    LcPtr::new(aws_lc_sys::EC_GROUP_new_by_curve_name(nid)).map_err(|_| Unspecified)
}

#[allow(non_snake_case)]
unsafe fn EC_POINT_from_bytes(
    ec_group: &LcPtr<*mut EC_GROUP>,
    bytes: &[u8],
) -> Result<LcPtr<*mut EC_POINT>, Unspecified> {
    let ec_point = LcPtr::new(EC_POINT_new(**ec_group)).map_err(|_| Unspecified)?;

    if 1 != aws_lc_sys::EC_POINT_oct2point(
        **ec_group,
        *ec_point,
        bytes.as_ptr(),
        bytes.len(),
        null_mut(),
    ) {
        return Err(Unspecified);
    }

    Ok(ec_point)
}

#[allow(non_snake_case)]
unsafe fn EC_POINT_to_bytes(
    ec_group: *const EC_GROUP,
    ec_point: *const EC_POINT,
    buf: &mut [u8; PUBLIC_KEY_MAX_LEN],
) -> Result<usize, Unspecified> {
    let pt_conv_form = aws_lc_sys::point_conversion_form_t::POINT_CONVERSION_UNCOMPRESSED;

    let out_len = aws_lc_sys::EC_POINT_point2oct(
        ec_group,
        ec_point,
        pt_conv_form,
        buf.as_mut_ptr().cast(),
        PUBLIC_KEY_MAX_LEN,
        null_mut(),
    );
    if out_len == 0 {
        return Err(Unspecified);
    }

    Ok(out_len)
}

#[allow(non_snake_case)]
unsafe fn ECDSA_SIG_to_asn1(ecdsa_sig: &LcPtr<*mut ECDSA_SIG>) -> Result<Signature, Unspecified> {
    let mut out_bytes = MaybeUninit::<*mut u8>::uninit();
    let mut out_len = MaybeUninit::<usize>::uninit();

    if 1 != ECDSA_SIG_to_bytes(out_bytes.as_mut_ptr(), out_len.as_mut_ptr(), **ecdsa_sig) {
        return Err(Unspecified);
    }
    let out_bytes = LcPtr::new(out_bytes.assume_init()).map_err(|_| Unspecified)?;
    let out_len = out_len.assume_init();

    Ok(Signature::new(|slice| {
        let out_bytes = slice::from_raw_parts(*out_bytes, out_len);
        slice[0..out_len].copy_from_slice(out_bytes);
        out_len
    }))
}

#[allow(non_snake_case)]
unsafe fn ECDSA_SIG_to_fixed(
    alg_id: &'static AlgorithmID,
    sig: &LcPtr<*mut ECDSA_SIG>,
) -> Result<Signature, Unspecified> {
    let expected_number_size = ecdsa_fixed_number_byte_size(alg_id);

    let r_bn = NonNullPtr::new(aws_lc_sys::ECDSA_SIG_get0_r(**sig)).map_err(|_| Unspecified)?;
    let mut r_buffer = [0u8; MAX_ECDSA_FIXED_NUMBER_BYTE_SIZE];
    let r_bytes = BIGNUM_to_be_bytes(r_bn, &mut r_buffer).map_err(|_| Unspecified)?;

    let s_bn = NonNullPtr::new(aws_lc_sys::ECDSA_SIG_get0_s(**sig)).map_err(|_| Unspecified)?;
    let mut s_buffer = [0u8; MAX_ECDSA_FIXED_NUMBER_BYTE_SIZE];
    let s_bytes = BIGNUM_to_be_bytes(s_bn, &mut s_buffer).map_err(|_| Unspecified)?;

    Ok(Signature::new(|slice| {
        let (r_start, r_end) = ((expected_number_size - r_bytes), expected_number_size);
        let (s_start, s_end) = (
            (2 * expected_number_size - s_bytes),
            2 * expected_number_size,
        );

        slice[r_start..r_end].copy_from_slice(&r_buffer[0..r_bytes]);
        slice[s_start..s_end].copy_from_slice(&s_buffer[0..s_bytes]);
        2 * expected_number_size
    }))
}

#[allow(non_snake_case)]
unsafe fn ECDSA_SIG_from_asn1(signature: &[u8]) -> Result<LcPtr<*mut ECDSA_SIG>, Unspecified> {
    LcPtr::new(ECDSA_SIG_from_bytes(signature.as_ptr(), signature.len())).map_err(|_| Unspecified)
}

const MAX_ECDSA_FIXED_NUMBER_BYTE_SIZE: usize = 48;

#[inline]
const fn ecdsa_fixed_number_byte_size(alg_id: &'static AlgorithmID) -> usize {
    match alg_id {
        AlgorithmID::ECDSA_P256 => 32,
        AlgorithmID::ECDSA_P384 => 48,
    }
}

#[allow(non_snake_case)]
unsafe fn ECDSA_SIG_from_fixed(
    alg_id: &'static AlgorithmID,
    signature: &[u8],
) -> Result<LcPtr<*mut ECDSA_SIG>, Unspecified> {
    let num_size_bytes = ecdsa_fixed_number_byte_size(alg_id);
    if signature.len() != 2 * num_size_bytes {
        return Err(Unspecified);
    }
    let r_bn = BIGNUM_from_be_bytes(&signature[..num_size_bytes])?;
    let s_bn = BIGNUM_from_be_bytes(&signature[num_size_bytes..])?;

    let ecdsa_sig = LcPtr::new(ECDSA_SIG_new()).map_err(|_| Unspecified)?;

    if 1 != ECDSA_SIG_set0(*ecdsa_sig, *r_bn, *s_bn) {
        return Err(Unspecified);
    }
    r_bn.detach();
    s_bn.detach();

    Ok(ecdsa_sig)
}

#[allow(non_snake_case)]
unsafe fn BIGNUM_to_be_bytes(
    bignum: NonNullPtr<*mut BIGNUM>,
    bytes: &mut [u8],
) -> Result<usize, Unspecified> {
    let bn_bytes = BN_num_bytes(*bignum);
    if bn_bytes > bytes.len() as c_uint {
        return Err(Unspecified);
    }

    let bn_bytes = BN_bn2bin(*bignum, bytes.as_mut_ptr());
    if bn_bytes == 0 {
        return Err(Unspecified);
    }
    Ok(bn_bytes)
}

#[allow(non_snake_case)]
unsafe fn BIGNUM_from_be_bytes(bytes: &[u8]) -> Result<DetachableLcPtr<*mut BIGNUM>, Unspecified> {
    DetachableLcPtr::new(BN_bin2bn(bytes.as_ptr(), bytes.len(), null_mut()))
        .map_err(|_| Unspecified)
}

#[cfg(test)]
mod tests {
    use crate::ec::key_pair::EcdsaKeyPair;
    use crate::signature;
    use crate::signature::ECDSA_P256_SHA256_FIXED_SIGNING;
    use crate::test::from_dirty_hex;

    #[test]
    fn test_from_pkcs8() {
        let input = from_dirty_hex(
            r#"308187020100301306072a8648ce3d020106082a8648ce3d030107046d306b0201010420090460075f15d
            2a256248000fb02d83ad77593dde4ae59fc5e96142dffb2bd07a14403420004cf0d13a3a7577231ea1b66cf4
            021cd54f21f4ac4f5f2fdd28e05bc7d2bd099d1374cd08d2ef654d6f04498db462f73e0282058dd661a4c9b0
            437af3f7af6e724"#,
        );

        let result = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &input);
        result.unwrap();
    }

    #[test]
    fn test_ecdsa_asn1_verify() {
        /*
                Curve = P-256
        Digest = SHA256
        Msg = ""
        Q = 0430345fd47ea21a11129be651b0884bfac698377611acc9f689458e13b9ed7d4b9d7599a68dcf125e7f31055ccb374cd04f6d6fd2b217438a63f6f667d50ef2f0
        Sig = 30440220341f6779b75e98bb42e01095dd48356cbf9002dc704ac8bd2a8240b88d3796c60220555843b1b4e264fe6ffe6e2b705a376c05c09404303ffe5d2711f3e3b3a010a1
        Result = P (0 )
                 */

        let alg = &signature::ECDSA_P256_SHA256_ASN1;
        let msg = "";
        let public_key = from_dirty_hex(
            r#"0430345fd47ea21a11129be651b0884bfac698377611acc9f689458e1
        3b9ed7d4b9d7599a68dcf125e7f31055ccb374cd04f6d6fd2b217438a63f6f667d50ef2f0"#,
        );
        let sig = from_dirty_hex(
            r#"30440220341f6779b75e98bb42e01095dd48356cbf9002dc704ac8bd2a8240b8
        8d3796c60220555843b1b4e264fe6ffe6e2b705a376c05c09404303ffe5d2711f3e3b3a010a1"#,
        );
        let actual_result =
            signature::UnparsedPublicKey::new(alg, &public_key).verify(msg.as_bytes(), &sig);
        assert!(actual_result.is_ok(), "Key: {}", hex::encode(public_key));
    }
}
