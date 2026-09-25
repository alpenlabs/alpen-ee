//! Console-side mirrors for values whose own serde shape reflects badly.
//!
//! Each mirror redeclares a foreign type's fields under the same names, so
//! what the console shows is unchanged, and marks its byte vectors as bytes so
//! they reflect as one value instead of one per byte. See
//! [`Mirror`](super::reflect::Mirror).

use serde::{Deserialize, Serialize};
use zkaleido::{Proof, ProofMetadata, ProofReceipt, ProofReceiptWithMetadata, PublicValues};

use super::reflect::Mirror;

/// [`ProofReceiptWithMetadata`] with its proof and public values as bytes.
///
/// The real type's `Proof` and `PublicValues` are plain `Vec<u8>` newtypes with
/// a derived `Serialize`, so through serde a compressed SP1 proof would reflect
/// as a million integers. The metadata's shape is fine and passes through.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProofReceiptMirror {
    receipt: ReceiptMirror,
    metadata: ProofMetadata,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ReceiptMirror {
    #[serde(with = "serde_bytes")]
    proof: Vec<u8>,
    #[serde(with = "serde_bytes")]
    public_values: Vec<u8>,
}

impl Mirror<ProofReceiptWithMetadata> for ProofReceiptMirror {
    fn mirror(value: &ProofReceiptWithMetadata) -> Self {
        let receipt = value.receipt();
        Self {
            receipt: ReceiptMirror {
                proof: receipt.proof().as_bytes().to_vec(),
                public_values: receipt.public_values().as_bytes().to_vec(),
            },
            metadata: value.metadata().clone(),
        }
    }

    fn restore(self) -> ProofReceiptWithMetadata {
        let receipt = ProofReceipt::new(
            Proof::new(self.receipt.proof),
            PublicValues::new(self.receipt.public_values),
        );
        ProofReceiptWithMetadata::new(receipt, self.metadata)
    }
}

#[cfg(test)]
mod tests {
    use zkaleido::{ProgramId, ProofType, ZkVm};

    use super::{
        super::{
            reflect::{MirrorReflector, SerdeReflector, ValueReflector},
            value::FieldValue,
        },
        *,
    };

    fn receipt(proof_len: usize) -> ProofReceiptWithMetadata {
        let receipt = ProofReceipt::new(
            Proof::new((0..proof_len).map(|i| i as u8).collect()),
            PublicValues::new(vec![7; 16]),
        );
        let metadata = ProofMetadata::new(
            ZkVm::Native,
            ProgramId([9; 32]),
            "1.2.3",
            ProofType::Groth16,
        );
        ProofReceiptWithMetadata::new(receipt, metadata)
    }

    /// The whole point: the proof crosses as one byte string, not a list.
    #[test]
    fn the_proof_reflects_as_bytes_under_the_same_field_names() {
        let value = receipt(1024);
        let reflected = MirrorReflector::<ProofReceiptMirror>::to_value(&value).unwrap();

        let inner = reflected.get("receipt").expect("receipt field");
        match inner.get("proof") {
            Some(FieldValue::Bytes(bytes)) => assert_eq!(bytes.len(), 1024),
            other => panic!("proof did not reflect as bytes: {other:?}"),
        }
        assert!(matches!(
            inner.get("public_values"),
            Some(FieldValue::Bytes(b)) if b.len() == 16
        ));

        // The plain serde shape of the same value is what the mirror avoids.
        let plain = SerdeReflector::to_value(&value).unwrap();
        assert!(matches!(
            plain.get("receipt").and_then(|r| r.get("proof")),
            Some(FieldValue::List(items)) if items.len() == 1024
        ));

        // Field names match the real type's, so nothing the console showed
        // before is renamed.
        let names = |v: &FieldValue| {
            v.fields()
                .unwrap()
                .into_iter()
                .map(|(n, _)| n.to_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(names(&reflected), names(&plain));
        assert_eq!(
            names(reflected.get("metadata").unwrap()),
            names(plain.get("metadata").unwrap())
        );
    }

    #[test]
    fn a_receipt_survives_the_round_trip_exactly() {
        let value = receipt(4096);
        let reflected = MirrorReflector::<ProofReceiptMirror>::to_value(&value).unwrap();
        let restored: ProofReceiptWithMetadata =
            MirrorReflector::<ProofReceiptMirror>::from_value(&reflected).unwrap();
        assert_eq!(restored, value);

        // The canonical form is stable, which is what staging checks.
        let again = MirrorReflector::<ProofReceiptMirror>::to_value(&restored).unwrap();
        assert_eq!(again, reflected);
    }
}
