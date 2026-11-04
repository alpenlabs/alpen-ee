//! Admission checks for gossip packages received from peers.

use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

use strata_primitives::Buf32;

use crate::AlpenGossipPackage;

/// How a node treats the gossip packages its peers send it.
///
/// Every check runs on the connection that received the package, so a peer
/// that floods us only slows its own connection, and only packages that pass
/// reach the node's shared gossip task.
#[derive(Clone, Debug)]
pub(crate) enum GossipIngress {
    /// Drops every package unchecked.
    ///
    /// The sequencer makes the blocks, so its peers have nothing to tell it.
    Ignore,

    /// Admits a package only if the sequencer signed it and it carries a
    /// block newer than any admitted so far.
    Verify {
        sequencer_pubkey: Buf32,

        /// Highest sequence number admitted, shared by all connections.
        highest_seq_no: Arc<AtomicU64>,
    },
}

/// What a connection does with a package after [`GossipIngress::check`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IngressVerdict {
    /// Pass the package to the node.
    Admit,

    /// Discard the package and keep the peer.
    Drop,

    /// Discard the package and disconnect the peer.
    Reject(&'static str),
}

impl GossipIngress {
    pub(crate) fn verify(sequencer_pubkey: Buf32) -> Self {
        Self::Verify {
            sequencer_pubkey,
            highest_seq_no: Arc::new(AtomicU64::new(0)),
        }
    }

    pub(crate) fn check(&self, package: &AlpenGossipPackage) -> IngressVerdict {
        let Self::Verify {
            sequencer_pubkey,
            highest_seq_no,
        } = self
        else {
            return IngressVerdict::Drop;
        };

        // Honest peers on this network only relay packages they verified, so
        // a package under any other key means the peer is hostile or on
        // another network.
        if package.public_key() != sequencer_pubkey {
            return IngressVerdict::Reject("unexpected public key");
        }

        // Peers relay the same block in parallel, so a stale package is
        // normal and not a reason to drop the peer. Checking it before the
        // signature keeps replays of old blocks cheap.
        let seq_no = package.message().seq_no();
        if seq_no <= highest_seq_no.load(Ordering::Relaxed) {
            return IngressVerdict::Drop;
        }

        if !package.validate_signature() {
            return IngressVerdict::Reject("invalid signature");
        }

        // Another connection may have admitted the same block while this one
        // was verifying it.
        if highest_seq_no.fetch_max(seq_no, Ordering::Relaxed) >= seq_no {
            return IngressVerdict::Drop;
        }

        IngressVerdict::Admit
    }
}

#[cfg(test)]
mod tests {
    use reth_primitives_traits::Header;
    use secp256k1::{Keypair, Secp256k1, SecretKey};
    use strata_primitives::buf::Buf64;

    use super::*;
    use crate::AlpenGossipMessage;

    const SEQUENCER_PRIVKEY: [u8; 32] = [1; 32];
    const OTHER_PRIVKEY: [u8; 32] = [2; 32];

    fn pubkey_of(privkey: [u8; 32]) -> Buf32 {
        let secp = Secp256k1::signing_only();
        let secret_key = SecretKey::from_slice(&privkey).expect("valid private key");
        let (x_only, _parity) = Keypair::from_secret_key(&secp, &secret_key).x_only_public_key();
        Buf32(x_only.serialize())
    }

    fn signed_package(privkey: [u8; 32], seq_no: u64) -> AlpenGossipPackage {
        AlpenGossipMessage::new(Header::default(), seq_no)
            .into_package(pubkey_of(privkey), Buf32(privkey))
    }

    fn sequencer_ingress() -> GossipIngress {
        GossipIngress::verify(pubkey_of(SEQUENCER_PRIVKEY))
    }

    #[test]
    fn test_ignore_drops_valid_package() {
        let package = signed_package(SEQUENCER_PRIVKEY, 1);
        assert_eq!(GossipIngress::Ignore.check(&package), IngressVerdict::Drop);
    }

    #[test]
    fn test_admits_new_sequencer_package() {
        let package = signed_package(SEQUENCER_PRIVKEY, 1);
        assert_eq!(sequencer_ingress().check(&package), IngressVerdict::Admit);
    }

    #[test]
    fn test_rejects_package_from_other_key() {
        let package = signed_package(OTHER_PRIVKEY, 1);
        assert_eq!(
            sequencer_ingress().check(&package),
            IngressVerdict::Reject("unexpected public key")
        );
    }

    #[test]
    fn test_rejects_sequencer_key_with_forged_signature() {
        let message = AlpenGossipMessage::new(Header::default(), 1);
        let package = AlpenGossipPackage::new(message, pubkey_of(SEQUENCER_PRIVKEY), Buf64::zero());
        assert_eq!(
            sequencer_ingress().check(&package),
            IngressVerdict::Reject("invalid signature")
        );
    }

    #[test]
    fn test_forged_signature_does_not_advance_seq_no() {
        let ingress = sequencer_ingress();
        let forged = AlpenGossipPackage::new(
            AlpenGossipMessage::new(Header::default(), u64::MAX),
            pubkey_of(SEQUENCER_PRIVKEY),
            Buf64::zero(),
        );
        ingress.check(&forged);

        let package = signed_package(SEQUENCER_PRIVKEY, 1);
        assert_eq!(ingress.check(&package), IngressVerdict::Admit);
    }

    #[test]
    fn test_drops_repeated_and_stale_packages() {
        let ingress = sequencer_ingress();
        assert_eq!(
            ingress.check(&signed_package(SEQUENCER_PRIVKEY, 5)),
            IngressVerdict::Admit
        );
        assert_eq!(
            ingress.check(&signed_package(SEQUENCER_PRIVKEY, 5)),
            IngressVerdict::Drop
        );
        assert_eq!(
            ingress.check(&signed_package(SEQUENCER_PRIVKEY, 4)),
            IngressVerdict::Drop
        );
    }

    #[test]
    fn test_stale_check_is_shared_across_clones() {
        let ingress = sequencer_ingress();
        let other_connection = ingress.clone();
        assert_eq!(
            ingress.check(&signed_package(SEQUENCER_PRIVKEY, 3)),
            IngressVerdict::Admit
        );
        assert_eq!(
            other_connection.check(&signed_package(SEQUENCER_PRIVKEY, 3)),
            IngressVerdict::Drop
        );
    }
}
