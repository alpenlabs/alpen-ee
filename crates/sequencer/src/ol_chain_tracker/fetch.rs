//! Fetches the OL blocks that extend a block the tracker already holds.

use alpen_common::{get_inbox_messages_checked, OLBlockData, SequencerOLClient};
use eyre::eyre;
use strata_identifiers::OLBlockCommitment;

/// The OL blocks after a tracked block, or the block the OL has in its place.
#[derive(Debug)]
pub(super) enum Extension {
    /// Blocks after the tracked block, one per slot, in slot order.
    Blocks(Vec<OLBlockData>),
    /// The OL's block at the tracked block's slot, which is not the tracked block.
    Diverged(OLBlockCommitment),
}

/// Fetches the OL blocks in slots `base.slot() + 1 ..= max_slot` and checks that they
/// build on `base`.
///
/// The check reads the parent of the first new block instead of fetching `base` again.
/// An OL node promoted from checkpoint sync holds no block bodies at or below its history
/// anchor, so it cannot serve `base` when `base` is that anchor.
pub(super) async fn fetch_blocks_after(
    client: &impl SequencerOLClient,
    base: OLBlockCommitment,
    max_slot: u64,
) -> eyre::Result<Extension> {
    if max_slot <= base.slot() {
        return Err(eyre!(
            "no slots to fetch after base (base slot {}, max slot {max_slot})",
            base.slot()
        ));
    }
    let first_slot = base.slot() + 1;

    let first_link = client.get_block_link(first_slot).await?;
    if first_link.parent_blkid != *base.blkid() {
        return Ok(Extension::Diverged(OLBlockCommitment::new(
            base.slot(),
            first_link.parent_blkid,
        )));
    }

    let blocks = get_inbox_messages_checked(client, first_slot, max_slot).await?;

    // The parent check covers these blocks only if they start at the block whose
    // header was checked.
    let first_block = blocks
        .first()
        .expect("checked fetch returns one block per slot")
        .commitment;
    if first_block != first_link.commitment {
        return Err(eyre!(
            "OL returned different blocks at slot {first_slot} (header {:?}, summary {first_block:?})",
            first_link.commitment
        ));
    }

    Ok(Extension::Blocks(blocks))
}

#[cfg(test)]
mod tests {
    use alpen_common::MockSequencerOLClient;

    use super::*;
    use crate::ol_chain_tracker::test_utils::{
        create_block_data_chain, make_block, make_block_link, make_block_with_id,
    };

    #[tokio::test]
    async fn fetches_blocks_after_base_without_refetching_base() {
        let base = make_block(10);
        let new_blocks: Vec<_> = (11..=13).map(make_block).collect();
        let block_data = create_block_data_chain(&new_blocks, 0);

        let mut mock_client = MockSequencerOLClient::new();
        mock_client
            .expect_get_block_link()
            .withf(|slot| *slot == 11)
            .times(1)
            .returning(move |_| Ok(make_block_link(make_block(11), base)));
        mock_client
            .expect_get_inbox_messages()
            .withf(|min, max| *min == 11 && *max == 13)
            .times(1)
            .returning(move |_, _| Ok(block_data.clone()));

        let extension = fetch_blocks_after(&mock_client, base, 13).await.unwrap();

        let Extension::Blocks(blocks) = extension else {
            panic!("expected Blocks, got {extension:?}");
        };
        let slots: Vec<_> = blocks.iter().map(|b| b.commitment.slot()).collect();
        assert_eq!(slots, vec![11, 12, 13]);
    }

    #[tokio::test]
    async fn reports_divergence_when_parent_differs() {
        let base = make_block_with_id(10, 0xAA);
        let remote_at_10 = make_block_with_id(10, 0xBB);

        let mut mock_client = MockSequencerOLClient::new();
        mock_client
            .expect_get_block_link()
            .times(1)
            .returning(move |_| Ok(make_block_link(make_block(11), remote_at_10)));

        let extension = fetch_blocks_after(&mock_client, base, 13).await.unwrap();

        let Extension::Diverged(remote) = extension else {
            panic!("expected Diverged, got {extension:?}");
        };
        assert_eq!(remote, remote_at_10);
    }

    #[tokio::test]
    async fn errors_when_summary_differs_from_checked_header() {
        let base = make_block(10);
        let block_data = create_block_data_chain(&[make_block_with_id(11, 0xCC)], 0);

        let mut mock_client = MockSequencerOLClient::new();
        mock_client
            .expect_get_block_link()
            .times(1)
            .returning(move |_| Ok(make_block_link(make_block(11), base)));
        mock_client
            .expect_get_inbox_messages()
            .times(1)
            .returning(move |_, _| Ok(block_data.clone()));

        let result = fetch_blocks_after(&mock_client, base, 11).await;

        assert!(result
            .unwrap_err()
            .to_string()
            .contains("OL returned different blocks at slot 11"));
    }

    #[tokio::test]
    async fn errors_when_nothing_to_fetch() {
        let base = make_block(10);
        let mock_client = MockSequencerOLClient::new();

        let result = fetch_blocks_after(&mock_client, base, 10).await;

        assert!(result
            .unwrap_err()
            .to_string()
            .contains("no slots to fetch after base"));
    }
}
