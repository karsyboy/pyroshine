use aes::cipher::Array;
use aes::cipher::BlockModeDecrypt;
use aes::cipher::BlockModeEncrypt;
use aes::cipher::KeyIvInit;
use aes_gcm::{
	Aes128Gcm, Key,
	aead::{Aead, KeyInit, Nonce},
};
use inout::block_padding::Pkcs7;

pub(crate) fn encrypt(plaintext: &[u8], key: &[u8], iv: &[u8], tag: &mut [u8]) -> Result<Vec<u8>, aes_gcm::Error> {
	let key: &Key<Aes128Gcm> = key.try_into().map_err(|_| aes_gcm::Error)?;
	let nonce: &Nonce<Aes128Gcm> = iv.try_into().map_err(|_| aes_gcm::Error)?;
	let cipher = Aes128Gcm::new(key);

	// In OpenSSL, encrypting with GCM returns ciphertext usually without tag appended if you use `tag()` to retrieve it separate.
	// aes-gcm crate append tag to ciphertext.
	let mut ciphertext = cipher.encrypt(nonce, plaintext)?;

	// Split tag from ciphertext
	let tag_len = 16;
	let len = ciphertext.len();
	if len < tag_len {
		return Err(aes_gcm::Error);
	}
	let actual_ciphertext_len = len - tag_len;

	tag.copy_from_slice(&ciphertext[actual_ciphertext_len..]);
	ciphertext.truncate(actual_ciphertext_len);

	Ok(ciphertext)
}

pub(crate) fn decrypt(ciphertext: &[u8], key: &[u8], iv: &[u8], tag: &[u8]) -> Result<Vec<u8>, aes_gcm::Error> {
	let key: &Key<Aes128Gcm> = key.try_into().map_err(|_| aes_gcm::Error)?;
	let nonce: &Nonce<Aes128Gcm> = iv.try_into().map_err(|_| aes_gcm::Error)?;
	let cipher = Aes128Gcm::new(key);

	// Append tag to ciphertext for aes-gcm crate
	let mut payload = Vec::with_capacity(ciphertext.len() + tag.len());
	payload.extend_from_slice(ciphertext);
	payload.extend_from_slice(tag);

	cipher.decrypt(nonce, payload.as_ref())
}

pub(crate) fn encrypt_cbc(data: &[u8], key: &[u8], iv: &[u8]) -> Result<Vec<u8>, String> {
	let key: &Array<u8, _> = key.try_into().map_err(|_| "Invalid CBC key length")?;
	let iv: &Array<u8, _> = iv.try_into().map_err(|_| "Invalid CBC IV length")?;

	let cipher = cbc::Encryptor::<aes::Aes128>::new(key, iv);

	let pad_len = 16 - (data.len() % 16);
	let mut buffer = vec![0u8; data.len() + pad_len];
	buffer[..data.len()].copy_from_slice(data);

	let ct = cipher
		.encrypt_padded::<Pkcs7>(&mut buffer, data.len())
		.map_err(|_| "Buffer too small for CBC encryption")?;
	Ok(ct.to_vec())
}

/// AES-128-CBC decryption with PKCS#7 padding removed.
pub(crate) fn decrypt_cbc(data: &[u8], key: &[u8], iv: &[u8]) -> Result<Vec<u8>, String> {
	let key: &Array<u8, _> = key.try_into().map_err(|_| "Invalid CBC key length")?;
	let iv: &Array<u8, _> = iv.try_into().map_err(|_| "Invalid CBC IV length")?;
	if data.is_empty() || !data.len().is_multiple_of(16) {
		return Err("CBC ciphertext is not a whole number of blocks".into());
	}

	let cipher = cbc::Decryptor::<aes::Aes128>::new(key, iv);
	let mut buffer = data.to_vec();
	let plaintext = cipher
		.decrypt_padded::<Pkcs7>(&mut buffer)
		.map_err(|_| "Invalid CBC padding")?;
	let len = plaintext.len();
	buffer.truncate(len);
	Ok(buffer)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn cbc_round_trips_every_padding_length() {
		let key = [7u8; 16];
		let iv = [9u8; 16];
		for len in 0..40 {
			let data: Vec<u8> = (0..len).map(|i| i as u8).collect();
			let sealed = encrypt_cbc(&data, &key, &iv).unwrap();
			assert_eq!(decrypt_cbc(&sealed, &key, &iv).unwrap(), data, "len {len}");
		}
		assert!(decrypt_cbc(&[0; 15], &key, &iv).is_err());
		assert!(decrypt_cbc(&[], &key, &iv).is_err());
	}
}
