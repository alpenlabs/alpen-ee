#[cfg(target_os = "linux")]
use std::io;
use std::str::FromStr;

use aes_gcm_siv::{aead::AeadMutInPlace, Aes256GcmSiv, KeyInit, Nonce, Tag};
use alloy::{network::EthereumWallet, signers::local::PrivateKeySigner};
use bdk_wallet::{
    bitcoin::{
        bip32::{DerivationPath, Xpriv},
        secp256k1::SECP256K1,
        Network,
    },
    CreateParams, KeychainKind, LoadParams, Wallet,
};
use bip39::{Language, Mnemonic};
use dialoguer::{Confirm, Input, Select};
use password::{HashVersion, IncorrectPassword, Password};
use rand_core::{CryptoRngCore, OsRng};
use sha2::{Digest, Sha256};
#[cfg(feature = "test-mode")]
use shrex::Hex;
use terrors::OneOf;
use zeroize::Zeroizing;

use crate::constants::{
    AES_NONCE_LEN, AES_TAG_LEN, BIP44_ALPEN_EVM_WALLET_PATH, LANGUAGE_CODE_LEN, PW_SALT_LEN,
    SEED_LEN,
};

// One supported mnemonic language: its display name, the `bip39` variant, and its on-disk code.
//
// `code` has to be assigned by us, by hand: bip39::Language has no #[repr] or documented
// discriminant values, so even `language as u8` would silently ride on the crate's internal
// declaration order -- not a stable API guarantee, and liable to shift on a crate version bump.
// Our own codes are at least ones we control: never reuse or reassign an existing language's
// code, since it's baked into already-encrypted seed files. LANGUAGES' order only controls the
// language-selection prompt's display order and is safe to change freely.
struct LanguageEntry {
    name: &'static str,
    language: Language,
    code: u8,
}

const LANGUAGES: &[LanguageEntry] = &[
    LanguageEntry {
        name: "English",
        language: Language::English,
        code: 0,
    },
    LanguageEntry {
        name: "Simplified Chinese",
        language: Language::SimplifiedChinese,
        code: 1,
    },
    LanguageEntry {
        name: "Traditional Chinese",
        language: Language::TraditionalChinese,
        code: 2,
    },
    LanguageEntry {
        name: "Czech",
        language: Language::Czech,
        code: 3,
    },
    LanguageEntry {
        name: "French",
        language: Language::French,
        code: 4,
    },
    LanguageEntry {
        name: "Italian",
        language: Language::Italian,
        code: 5,
    },
    LanguageEntry {
        name: "Japanese",
        language: Language::Japanese,
        code: 6,
    },
    LanguageEntry {
        name: "Korean",
        language: Language::Korean,
        code: 7,
    },
    LanguageEntry {
        name: "Portuguese",
        language: Language::Portuguese,
        code: 8,
    },
    LanguageEntry {
        name: "Spanish",
        language: Language::Spanish,
        code: 9,
    },
];

fn language_code(language: Language) -> u8 {
    LANGUAGES
        .iter()
        .find(|entry| entry.language == language)
        .map(|entry| entry.code)
        .expect("all bip39 Language variants are listed in LANGUAGES")
}

fn language_from_code(code: u8) -> Option<Language> {
    LANGUAGES
        .iter()
        .find(|entry| entry.code == code)
        .map(|entry| entry.language)
}

#[expect(
    missing_debug_implementations,
    reason = "Struct contains sensitive wallet parameters that should not be debug printed"
)]
pub struct BaseWallet(LoadParams, CreateParams);

impl BaseWallet {
    pub fn split(self) -> (LoadParams, CreateParams) {
        (self.0, self.1)
    }
}

#[derive(Clone)]
#[cfg_attr(feature = "test-mode", derive(Debug))]
#[cfg_attr(
    not(feature = "test-mode"),
    expect(missing_debug_implementations, reason = "debug not required")
)]
pub struct Seed {
    // NOTE: This is not a BIP39 seed, instead random bytes of entropy.
    entropy: Zeroizing<[u8; SEED_LEN]>,

    // The mnemonic language this entropy is always encoded/derived with, so a mnemonic printed
    // by `print_mnemonic` and the keys derived by `signet_wallet`/`get_alpen_wallet` agree with
    // what a standard wallet computes from that same printed mnemonic.
    language: Language,
}

impl Seed {
    #[cfg(feature = "test-mode")]
    pub fn from_file(bytes: Hex<[u8; SEED_LEN]>) -> Self {
        Self {
            entropy: Zeroizing::new(*bytes),
            language: Language::English,
        }
    }

    fn gen<R: CryptoRngCore>(rng: &mut R, language: Language) -> Self {
        let mut entropy = Zeroizing::new([0u8; SEED_LEN]);
        rng.fill_bytes(entropy.as_mut());
        Self { entropy, language }
    }

    pub fn print_mnemonic(&self) {
        let mnemonic =
            Mnemonic::from_entropy_in(self.language, self.entropy.as_ref()).expect("valid entropy");
        println!("{mnemonic}");
    }

    pub fn descriptor_recovery_key(&self) -> [u8; 32] {
        let mut hasher = <Sha256 as Digest>::new(); // this is to appease the analyzer
        hasher.update(b"alpen labs alpen descriptor recovery file 2024");
        hasher.update(self.entropy.as_slice());
        hasher.finalize().into()
    }

    pub fn encrypt<R: CryptoRngCore>(
        &self,
        password: &mut Password,
        rng: &mut R,
    ) -> Result<EncryptedSeed, OneOf<(argon2::Error, aes_gcm_siv::Error)>> {
        let mut buf = [0u8; EncryptedSeed::LEN];
        rng.fill_bytes(&mut buf[..PW_SALT_LEN + AES_NONCE_LEN]);

        let seed_encryption_key = password
            .seed_encryption_key(
                &buf[..PW_SALT_LEN].try_into().expect("cannot fail"),
                HashVersion::V0,
            )
            .map_err(OneOf::new)?;

        let (salt_and_nonce, rest) = buf.split_at_mut(PW_SALT_LEN + AES_NONCE_LEN);
        let (plaintext, _) = rest.split_at_mut(SEED_LEN + LANGUAGE_CODE_LEN);
        let (entropy, language_code_byte) = plaintext.split_at_mut(SEED_LEN);
        entropy.copy_from_slice(self.entropy.as_ref());
        language_code_byte[0] = language_code(self.language);

        let mut cipher = Aes256GcmSiv::new_from_slice(seed_encryption_key.as_ref())
            .expect("should be correct key size");
        let nonce = Nonce::from_slice(&salt_and_nonce[PW_SALT_LEN..]);
        let tag = cipher
            .encrypt_in_place_detached(nonce, &[], plaintext)
            .map_err(OneOf::new)?;
        buf[(EncryptedSeed::LEN - AES_TAG_LEN)..].copy_from_slice(tag.as_slice());
        Ok(EncryptedSeed(buf))
    }

    pub fn signet_wallet(&self) -> BaseWallet {
        let mnemonic =
            Mnemonic::from_entropy_in(self.language, self.entropy.as_ref()).expect("valid entropy");
        // We do not use a passphrase.
        let bip39_seed = mnemonic.to_seed("");
        let rootpriv = Xpriv::new_master(Network::Signet, &bip39_seed).expect("valid xpriv");
        let base_desc = format!("tr({rootpriv}/86h/0h/0h");
        let external_desc = format!("{base_desc}/0/*)");
        let internal_desc = format!("{base_desc}/1/*)");
        BaseWallet(
            Wallet::load()
                .descriptor(KeychainKind::External, Some(external_desc.clone()))
                .descriptor(KeychainKind::Internal, Some(internal_desc.clone()))
                .extract_keys(),
            Wallet::create(external_desc, internal_desc),
        )
    }

    pub fn get_alpen_wallet(&self) -> EthereumWallet {
        let derivation_path = DerivationPath::master().extend(BIP44_ALPEN_EVM_WALLET_PATH);

        let mnemonic =
            Mnemonic::from_entropy_in(self.language, self.entropy.as_ref()).expect("valid entropy");
        // We do not use a passphrase.
        let bip39_seed = mnemonic.to_seed("");
        // Network choice affects how extended public and private keys are serialized. See
        // https://github.com/bitcoin/bips/blob/master/bip-0032.mediawiki#serialization-format.
        // Given the popularity of MetaMask, we follow their example (they always hardcode mainnet)
        // and hardcode Network::Bitcoin (mainnet) for EVM-based wallet.
        let master_key = Xpriv::new_master(Network::Bitcoin, &bip39_seed).expect("valid xpriv");

        // Derive the child key for the given path
        let derived_key = master_key.derive_priv(SECP256K1, &derivation_path).unwrap();
        let signer =
            PrivateKeySigner::from_slice(derived_key.private_key.secret_bytes().as_slice())
                .expect("valid slice");

        EthereumWallet::from(signer)
    }
}

#[expect(
    missing_debug_implementations,
    reason = "Struct contains encrypted seed data that should not be debug printed"
)]
pub struct EncryptedSeed([u8; Self::LEN]);

impl EncryptedSeed {
    const LEN: usize = PW_SALT_LEN + AES_NONCE_LEN + SEED_LEN + LANGUAGE_CODE_LEN + AES_TAG_LEN;

    fn decrypt(
        mut self,
        password: &mut Password,
    ) -> Result<Seed, OneOf<(argon2::Error, aes_gcm_siv::Error)>> {
        let seed_encryption_key = password
            .seed_encryption_key(
                &self.0[..PW_SALT_LEN].try_into().expect("cannot fail"),
                HashVersion::V0,
            )
            .map_err(OneOf::new)?;

        let mut cipher = Aes256GcmSiv::new_from_slice(seed_encryption_key.as_ref())
            .expect("should be correct key size");
        let (salt_and_nonce, rest) = self.0.split_at_mut(PW_SALT_LEN + AES_NONCE_LEN);
        let (plaintext, tag) = rest.split_at_mut(SEED_LEN + LANGUAGE_CODE_LEN);
        let tag = Tag::from_slice(tag);
        let nonce = Nonce::from_slice(&salt_and_nonce[PW_SALT_LEN..]);

        let mut decrypted = Zeroizing::new([0u8; SEED_LEN + LANGUAGE_CODE_LEN]);
        decrypted.copy_from_slice(plaintext);

        cipher
            .decrypt_in_place_detached(nonce, &[], decrypted.as_mut(), tag)
            .map_err(OneOf::new)?;

        let mut entropy = Zeroizing::new([0u8; SEED_LEN]);
        entropy.copy_from_slice(&decrypted[..SEED_LEN]);
        let language = language_from_code(decrypted[SEED_LEN])
            .expect("encrypted seed's language code is written by our own encrypt()");

        Ok(Seed { entropy, language })
    }
}

pub fn load_or_create(
    persister: &impl EncryptedSeedPersister,
) -> Result<Seed, OneOf<LoadOrCreateErr>> {
    println!("Loading encrypted seed...");
    let maybe_encrypted_seed = persister.load().map_err(OneOf::broaden)?;
    if let Some(encrypted_seed) = maybe_encrypted_seed {
        println!("Opening wallet...");
        let mut password = Password::read(false).map_err(OneOf::new)?;
        match encrypted_seed.decrypt(&mut password) {
            Ok(seed) => {
                println!("Wallet unlocked");
                Ok(seed)
            }
            Err(e) => {
                let narrowed = e.narrow::<aes_gcm_siv::Error, _>();
                if let Ok(_aes_error) = narrowed {
                    return Err(OneOf::new(IncorrectPassword));
                }

                Err(narrowed.unwrap_err().broaden())
            }
        }
    } else {
        let restore = Confirm::new()
            .with_prompt("Do you want to restore a previously created wallet?")
            .interact()
            .map_err(OneOf::new)?;

        let seed = if restore {
            loop {
                let mnemonic: String = Input::new()
                    .with_prompt("Enter your mnemonic")
                    .interact_text()
                    .map_err(OneOf::new)?;

                let mnemonic = match Mnemonic::from_str(&mnemonic) {
                    Ok(m) => m,
                    Err(e) => {
                        println!("please try again: {e}");
                        continue;
                    }
                };
                let entropy = mnemonic.to_entropy();
                if entropy.len() != SEED_LEN {
                    println!("incorrect entropy length");
                    continue;
                }
                let mut buf = Zeroizing::new([0u8; SEED_LEN]);
                buf.copy_from_slice(&entropy);
                break Seed {
                    entropy: buf,
                    // The mnemonic's own language, not a default: this is the language a
                    // standard wallet would derive from these exact words, so it's the only
                    // choice that keeps Alpen's derivation consistent with the phrase the user
                    // actually typed in.
                    language: mnemonic.language(),
                };
            }
        } else {
            println!("Creating new wallet");
            let language_names: Vec<&str> = LANGUAGES.iter().map(|entry| entry.name).collect();
            let language_idx = Select::new()
                .with_prompt("Choose a language for your recovery mnemonic")
                .items(&language_names)
                .default(0)
                .interact()
                .map_err(OneOf::new)?;
            let language = LANGUAGES[language_idx].language;
            Seed::gen(&mut OsRng, language)
        };

        let mut password = Password::read(true).map_err(OneOf::new)?;
        let password_validation: Result<(), String> = password.validate();
        if let Err(feedback) = password_validation {
            println!("Password is weak. {feedback}");
        };
        let encrypted_seed = match seed.encrypt(&mut password, &mut OsRng) {
            Ok(es) => es,
            Err(e) => {
                let narrowed = e.narrow::<aes_gcm_siv::Error, _>();
                if let Ok(aes_error) = narrowed {
                    panic!("Failed to encrypt seed: {aes_error:?}");
                }

                return Err(narrowed.unwrap_err().broaden());
            }
        };
        persister.save(&encrypted_seed).map_err(OneOf::broaden)?;
        Ok(seed)
    }
}

#[cfg(not(target_os = "linux"))]
type PersisterErr = OneOf<(PlatformFailure, NoStorageAccess)>;

#[cfg(target_os = "linux")]
type PersisterErr = OneOf<(io::Error,)>;

#[cfg(target_os = "linux")]
type LoadOrCreateErr = (
    io::Error,
    dialoguer::Error,
    argon2::Error,
    IncorrectPassword,
);

#[cfg(not(target_os = "linux"))]
type LoadOrCreateErr = (
    PlatformFailure,
    NoStorageAccess,
    dialoguer::Error,
    argon2::Error,
    IncorrectPassword,
);

pub trait EncryptedSeedPersister {
    fn save(&self, seed: &EncryptedSeed) -> Result<(), PersisterErr>;
    fn load(&self) -> Result<Option<EncryptedSeed>, PersisterErr>;
    fn delete(&self) -> Result<(), PersisterErr>;
}

#[cfg(target_os = "linux")]
pub use file::*;

#[cfg(target_os = "linux")]
mod file;

#[cfg(not(target_os = "linux"))]
mod keychain;

#[cfg(not(target_os = "linux"))]
pub use keychain::*;

pub mod password;

#[cfg(test)]
mod test {
    use rand_core::OsRng;
    use sha2::digest::generic_array::GenericArray;

    use super::*;

    #[test]
    // Sanity checks on curve scalar construction, to ensure proper rejection
    // This treats zero as invalid (for ECDSA reasons)
    fn scalar_sanity_checks() {
        // This is the (big-endian) order of the `secp256k1` curve group
        // You can find it in, for example, section 2.4.1 of https://www.secg.org/sec2-v2.pdf
        let mut order: [u8; 32] = [
            0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
            0xFF, 0xFE, 0xBA, 0xAE, 0xDC, 0xE6, 0xAF, 0x48, 0xA0, 0x3B, 0xBF, 0xD2, 0x5E, 0x8C,
            0xD0, 0x36, 0x41, 0x41,
        ];

        // The scalar can't be zero
        assert!(PrivateKeySigner::from_field_bytes(GenericArray::from_slice(&[0u8; 32])).is_err());

        // The scalar can be well within the group order
        assert!(PrivateKeySigner::from_field_bytes(GenericArray::from_slice(&[1u8; 32])).is_ok());

        // The scalar can't equal the group order
        assert!(PrivateKeySigner::from_field_bytes(GenericArray::from_slice(&order)).is_err());

        // The scalar can't exceed the group order
        order[31] = 0x42;
        assert!(PrivateKeySigner::from_field_bytes(GenericArray::from_slice(&order)).is_err());
        assert!(
            PrivateKeySigner::from_field_bytes(GenericArray::from_slice(&[u8::MAX; 32])).is_err()
        );

        // The scalar can be _just_ under the group order
        order[31] = 0x40;
        assert!(PrivateKeySigner::from_field_bytes(GenericArray::from_slice(&order)).is_ok());
    }

    #[test]
    // Test valid seed encryption and decryption
    fn seed_encrypt_decrypt() {
        let mut password = Password::new(String::from("swordfish"));
        let seed = Seed::gen(&mut OsRng, Language::Spanish);

        let encrypted_seed = seed.encrypt(&mut password, &mut OsRng).unwrap();
        let decrypted_seed = encrypted_seed.decrypt(&mut password).unwrap();

        assert_eq!(seed.entropy, decrypted_seed.entropy);
        assert_eq!(seed.language, decrypted_seed.language);
    }

    #[test]
    // Using an evil password fails decryption
    fn evil_password() {
        let mut password = Password::new(String::from("swordfish"));
        let mut evil_password = Password::new(String::from("evil"));
        let seed = Seed::gen(&mut OsRng, Language::English);

        let encrypted_seed = seed.encrypt(&mut password, &mut OsRng).unwrap();

        assert!(encrypted_seed.decrypt(&mut evil_password).is_err());
    }

    #[test]
    // Using an evil salt fails decryption
    fn evil_salt() {
        let mut password = Password::new(String::from("swordfish"));
        let seed = Seed::gen(&mut OsRng, Language::English);

        let mut encrypted_seed = seed.encrypt(&mut password, &mut OsRng).unwrap();
        let index = 0;
        encrypted_seed.0[index] = !encrypted_seed.0[index];

        assert!(encrypted_seed.decrypt(&mut password).is_err());
    }

    #[test]
    // Using an evil nonce fails decryption
    fn evil_nonce() {
        let mut password = Password::new(String::from("swordfish"));
        let seed = Seed::gen(&mut OsRng, Language::English);

        let mut encrypted_seed = seed.encrypt(&mut password, &mut OsRng).unwrap();
        let index = PW_SALT_LEN;
        encrypted_seed.0[index] = !encrypted_seed.0[index];

        assert!(encrypted_seed.decrypt(&mut password).is_err());
    }

    #[test]
    // Using an evil seed fails decryption
    fn evil_seed() {
        let mut password = Password::new(String::from("swordfish"));
        let seed = Seed::gen(&mut OsRng, Language::English);

        let mut encrypted_seed = seed.encrypt(&mut password, &mut OsRng).unwrap();
        let index = PW_SALT_LEN + AES_NONCE_LEN;
        encrypted_seed.0[index] = !encrypted_seed.0[index];

        assert!(encrypted_seed.decrypt(&mut password).is_err());
    }

    #[test]
    // Using an evil tag fails decryption
    fn evil_tag() {
        let mut password = Password::new(String::from("swordfish"));
        let seed = Seed::gen(&mut OsRng, Language::English);

        let mut encrypted_seed = seed.encrypt(&mut password, &mut OsRng).unwrap();
        let index = PW_SALT_LEN + AES_NONCE_LEN + SEED_LEN;
        encrypted_seed.0[index] = !encrypted_seed.0[index];

        assert!(encrypted_seed.decrypt(&mut password).is_err());
    }

    #[test]
    // The on-disk language code must round-trip for every supported language, not just whichever
    // one other tests happen to exercise.
    fn language_code_round_trips_for_every_supported_language() {
        for entry in LANGUAGES {
            let code = language_code(entry.language);
            assert_eq!(code, entry.code, "language code for {} changed", entry.name);
            assert_eq!(
                language_from_code(code),
                Some(entry.language),
                "language code for {} did not round-trip",
                entry.name
            );
        }
    }

    #[test]
    // Test L2 wallet address matches the one from BIP39 tool (e.g. https://iancoleman.io/bip39/)
    // using the same menmonic and derivation path.
    fn test_l2_wallet_address() {
        let seed = Seed {
            entropy: [
                0xBA, 0xAE, 0xDC, 0xE6, 0xAF, 0x48, 0xA0, 0x3B, 0xBF, 0xD2, 0x5E, 0x8C, 0xD0, 0x36,
                0x41, 0x41,
            ]
            .into(),
            language: Language::English,
        };
        let l2wallet = seed.get_alpen_wallet();
        let address = l2wallet.default_signer().address().to_string();
        // BIP39 Mnemonic for `seed` should be:
        // rival ivory defy future meat build young envelope mimic like motion loan
        // The expected address is obtained using the BIP39 tool with the above mnemonic
        // and BIP44 derivation path m/44'/60'/0'/0/0.
        let expected_address = "0x4eEE6B504Bc2c47650bAa7d135DA10F2A469E582".to_string();
        assert_eq!(address, expected_address);
    }

    #[test]
    // BIP-86's official test vector for m/86'/0'/0'/0/0 (mnemonic "abandon abandon abandon
    // abandon abandon abandon abandon abandon abandon abandon abandon about", all-zero entropy).
    // Confirms the signet wallet is now derived via the standard BIP39 path (entropy -> mnemonic
    // -> PBKDF2 seed -> BIP32) instead of raw entropy, so any BIP-86-compliant wallet can recover
    // the same funds. https://github.com/bitcoin/bips/blob/master/bip-0086.mediawiki#test-vectors
    fn test_l1_signet_wallet_matches_bip86_test_vector() {
        let seed = Seed {
            entropy: [0u8; SEED_LEN].into(),
            language: Language::English,
        };
        let (_, create) = seed.signet_wallet().split();
        let mut wallet = create
            .network(Network::Signet)
            .create_wallet_no_persist()
            .expect("valid descriptor");
        let script_pubkey = wallet
            .reveal_next_address(KeychainKind::External)
            .address
            .script_pubkey();

        let expected_bytes = shrex::decode_alloc(
            "5120a60869f0dbcf1dc659c9cecbaf8050135ea9e8cdc487053f1dc6880949dc684c",
        )
        .expect("valid hex");
        assert_eq!(script_pubkey.as_bytes(), expected_bytes.as_slice());
    }

    #[test]
    // Finding #5: get_alpen_wallet used to always reconstruct the mnemonic in English before
    // hashing, regardless of the seed's actual language, diverging from what a real wallet
    // computes for a mnemonic in any other language. Now that Seed carries its own language and
    // get_alpen_wallet uses it, a Spanish-language seed must match a correct Spanish derivation.
    fn l2_wallet_matches_correct_derivation_for_non_english_mnemonic() {
        let seed = Seed {
            entropy: [0u8; SEED_LEN].into(),
            language: Language::Spanish,
        };

        // What Alpen actually computes.
        let alpen_address = seed.get_alpen_wallet().default_signer().address();

        // What a real external wallet would compute, given the Spanish mnemonic `alpen backup`
        // prints for this same seed.
        let spanish_mnemonic = Mnemonic::from_entropy_in(Language::Spanish, seed.entropy.as_ref())
            .expect("valid entropy");
        let correct_bip39_seed = spanish_mnemonic.to_seed("");
        let correct_root =
            Xpriv::new_master(Network::Bitcoin, &correct_bip39_seed).expect("valid xpriv");
        let derivation_path = DerivationPath::master().extend(BIP44_ALPEN_EVM_WALLET_PATH);
        let derived_key = correct_root
            .derive_priv(SECP256K1, &derivation_path)
            .unwrap();
        let correct_signer =
            PrivateKeySigner::from_slice(derived_key.private_key.secret_bytes().as_slice())
                .expect("valid slice");

        assert_eq!(alpen_address, correct_signer.address());
    }

    #[test]
    // L1 counterpart to l2_wallet_matches_correct_derivation_for_non_english_mnemonic.
    fn l1_signet_wallet_matches_correct_derivation_for_non_english_mnemonic() {
        let seed = Seed {
            entropy: [0u8; SEED_LEN].into(),
            language: Language::Spanish,
        };

        // What Alpen actually computes.
        let (_, create) = seed.signet_wallet().split();
        let mut wallet = create
            .network(Network::Signet)
            .create_wallet_no_persist()
            .expect("valid descriptor");
        let alpen_script_pubkey = wallet
            .reveal_next_address(KeychainKind::External)
            .address
            .script_pubkey();

        // What a real external wallet would compute, given the Spanish mnemonic `alpen backup`
        // prints for this same seed.
        let spanish_mnemonic = Mnemonic::from_entropy_in(Language::Spanish, seed.entropy.as_ref())
            .expect("valid entropy");
        let correct_bip39_seed = spanish_mnemonic.to_seed("");
        let correct_root =
            Xpriv::new_master(Network::Signet, &correct_bip39_seed).expect("valid xpriv");
        let base_desc = format!("tr({correct_root}/86h/0h/0h");
        let external_desc = format!("{base_desc}/0/*)");
        let internal_desc = format!("{base_desc}/1/*)");
        let mut correct_wallet = Wallet::create(external_desc, internal_desc)
            .network(Network::Signet)
            .create_wallet_no_persist()
            .expect("valid descriptor");
        let correct_script_pubkey = correct_wallet
            .reveal_next_address(KeychainKind::External)
            .address
            .script_pubkey();

        assert_eq!(alpen_script_pubkey, correct_script_pubkey);
    }
}
