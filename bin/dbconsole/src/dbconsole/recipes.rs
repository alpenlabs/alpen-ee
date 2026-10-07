//! The recipes shipped with the binary.
//!
//! A recipe is a rhai function in one of the files under `recipes/`,
//! embedded at build time and defined in every session before the first
//! input, so it is versioned and reviewed with the binary. A recipe stages
//! its edits and returns how many; it never commits. `.staged` is the
//! preview and `commit()` the act, which is the dry-run default the console
//! is built around. `.recipes` lists them with their doc comments.
//!
//! A recipe that touches more than one environment stages `node` first.
//! `commit()` applies environments in the order they were first staged, and
//! it cannot make them land together. What follows the node edits is either
//! a regenerable cache (`witness`) or prover work derived from node rows
//! (`prover`). The chain and batch recipes check `has_env` before they touch
//! an environment a full node lacks.

use super::session::Session;

/// The recipe files, in load order: helpers first, since the rest call them.
const FILES: [(&str, &str); 5] = [
    ("common.rhai", include_str!("../../recipes/common.rhai")),
    ("prover.rhai", include_str!("../../recipes/prover.rhai")),
    ("chain.rhai", include_str!("../../recipes/chain.rhai")),
    ("batches.rhai", include_str!("../../recipes/batches.rhai")),
    ("da.rhai", include_str!("../../recipes/da.rhai")),
];

/// Defines every shipped recipe in `session`.
pub(crate) fn install(session: &mut Session) -> eyre::Result<()> {
    for (name, src) in FILES {
        session.define_recipes(name, src)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::{atomic::AtomicBool, Arc};

    use alpen_database::test_db::TempDatadir;
    use alpen_dbconsole_core::{ConsoleDb, StagedOp};
    use rhai::{Dynamic, Map};

    use super::*;

    /// A read-write session over a datadir with a row in every table.
    fn seeded_session() -> (TempDatadir, Session) {
        session_over(TempDatadir::seeded())
    }

    /// A read-write session over a full node's datadir: only the node
    /// environment, genesis finalized, two blocks at the unfinalized tip.
    fn full_node_session() -> (TempDatadir, Session) {
        session_over(TempDatadir::full_node())
    }

    /// A read-write session over `datadir`. The datadir is returned with it
    /// and must outlive the session.
    fn session_over(datadir: TempDatadir) -> (TempDatadir, Session) {
        let db = ConsoleDb::attach_readwrite(&datadir).unwrap();
        let mut session = Session::new(db, Arc::new(AtomicBool::new(false)));
        install(&mut session).unwrap();
        (datadir, session)
    }

    fn int(session: &mut Session, src: &str) -> i64 {
        session.eval(src).unwrap().as_int().unwrap()
    }

    fn map(session: &mut Session, src: &str) -> Map {
        session.eval(src).unwrap().cast::<Map>()
    }

    fn staged_of<'a>(ops: &'a [StagedOp], table: &str) -> Vec<&'a StagedOp> {
        ops.iter().filter(|op| op.table() == table).collect()
    }

    #[test]
    fn every_recipe_file_compiles_and_is_listed_with_its_doc_comment() {
        let (_datadir, session) = seeded_session();
        let recipes = session.recipes();
        let names: Vec<_> = recipes.iter().map(|r| r.name.as_str()).collect();
        for expected in [
            "batch_summary",
            "broadcast_summary",
            "chain_summary",
            "drop_chain_above",
            "prover_abandon",
            "prover_delete",
            "prover_reset",
            "prover_summary",
            "prover_task",
            "revert_batches_from",
            "revert_chunks_from",
            "set_next_spec_version",
            "tip_height",
        ] {
            assert!(names.contains(&expected), "missing recipe {expected}");
        }
        for recipe in &recipes {
            assert!(
                !recipe.docs.is_empty(),
                "{} has no doc comment",
                recipe.name
            );
        }
    }

    #[test]
    fn the_summaries_describe_the_seeded_store() {
        let (_datadir, mut session) = seeded_session();

        let prover = map(&mut session, "prover_summary()");
        for kind in ["chunk", "acct"] {
            let by_status = prover.get(kind).unwrap().clone().cast::<Map>();
            assert_eq!(by_status.get("Pending").unwrap().as_int().unwrap(), 1);
        }

        let chain = map(&mut session, "chain_summary()");
        assert_eq!(chain.get("tip_height").unwrap().as_int().unwrap(), 2);
        assert_eq!(chain.get("finalized_height").unwrap().as_int().unwrap(), 2);
        assert_eq!(chain.get("exec_blocks").unwrap().as_int().unwrap(), 3);
        assert_eq!(chain.get("batches").unwrap().as_int().unwrap(), 1);
        assert_eq!(chain.get("chunks").unwrap().as_int().unwrap(), 1);

        let batches = map(&mut session, "batch_summary()");
        let by_status = batches.get("batches").unwrap().clone().cast::<Map>();
        assert_eq!(by_status.get("Genesis").unwrap().as_int().unwrap(), 1);
        let chunks = batches.get("chunks").unwrap().clone().cast::<Map>();
        assert_eq!(
            chunks.get("ProvingNotStarted").unwrap().as_int().unwrap(),
            1
        );

        let da = map(&mut session, "broadcast_summary()");
        assert_eq!(da.get("queued").unwrap().as_int().unwrap(), 1);
        assert_eq!(da.get("active_chains").unwrap().as_int().unwrap(), 1);
        let status = da.get("by_status").unwrap().clone().cast::<Map>();
        assert_eq!(status.get("unpublished").unwrap().as_int().unwrap(), 1);

        // Summaries stage nothing.
        assert!(session.db().staged().is_empty());
    }

    /// The keys of the two seeded tasks: the chunk task and the account task,
    /// each the only row of its table.
    fn seeded_tasks(session: &mut Session) -> (String, String) {
        let key = |session: &mut Session, table: &str| {
            session
                .eval(&format!(r#"first("{table}").key"#))
                .unwrap()
                .to_string()
        };
        let chunk = key(session, "ChunkProverTaskSchema");
        let acct = key(session, "AcctProverTaskSchema");
        (chunk, acct)
    }

    #[test]
    fn prover_reset_and_abandon_stage_a_status_change_and_nothing_else() {
        let (_datadir, mut session) = seeded_session();
        let (chunk, acct) = seeded_tasks(&mut session);
        assert_eq!(
            int(
                &mut session,
                &format!(r#"prover_reset("ChunkProverTaskSchema", "{chunk}")"#)
            ),
            1
        );
        let staged = session.db().staged();
        assert_eq!(staged.len(), 1);
        assert!(matches!(&staged[0], StagedOp::Put { table, record, .. }
            if *table == "ChunkProverTaskSchema" && record.value.get("status").unwrap().to_string() == "Pending"));
        session.db().abort();

        assert_eq!(
            int(
                &mut session,
                &format!(r#"prover_abandon("AcctProverTaskSchema", "{acct}", "operator gave up")"#)
            ),
            1
        );
        let staged = session.db().staged();
        let StagedOp::Put { record, .. } = &staged[0] else {
            panic!("expected a put")
        };
        assert!(
            record
                .value
                .get("status")
                .unwrap()
                .to_string()
                .contains("operator gave up"),
            "{}",
            record.value
        );
    }

    /// The seeded chunk task points at the seeded chunk receipt and the
    /// account task at the account receipt; a task copied to a range nothing
    /// was proved for has none, and a key that is not a task key is refused
    /// by the table rather than guessed at.
    #[test]
    fn prover_task_finds_the_receipt_behind_a_task_key() {
        let (_datadir, mut session) = seeded_session();
        let (chunk, acct) = seeded_tasks(&mut session);

        let shown = map(
            &mut session,
            &format!(r#"prover_task("ChunkProverTaskSchema", "{chunk}")"#),
        );
        assert!(shown.get("task").unwrap().is_map());
        assert!(
            shown.get("receipt").unwrap().is_map(),
            "chunk receipt not found"
        );
        let shown = map(
            &mut session,
            &format!(r#"prover_task("prover/AcctProverTaskSchema", "{acct}")"#),
        );
        assert!(
            shown.get("receipt").unwrap().is_map(),
            "account receipt not found"
        );

        let unproved = unproved_chunk_task(&mut session, &chunk);
        let shown = map(
            &mut session,
            &format!(r#"prover_task("ChunkProverTaskSchema", "{unproved}")"#),
        );
        assert!(shown.get("task").unwrap().is_map());
        assert!(shown.get("receipt").unwrap().is_unit());

        let err = session
            .eval(r#"prover_task("ChunkProverTaskSchema", "010203")"#)
            .unwrap_err();
        assert!(err.to_string().contains("task key"), "{err}");
        assert!(session.db().staged().is_empty());
    }

    /// Writes a copy of the seeded chunk task over a range no receipt covers
    /// and returns its key: the same version, hashes of their own.
    fn unproved_chunk_task(session: &mut Session, chunk: &str) -> String {
        let key = format!("0000{}{}", "aa".repeat(32), "bb".repeat(32));
        let _ = session
            .eval(&format!(
                r#"
                put("ChunkProverTaskSchema", "{key}", edit("ChunkProverTaskSchema", "{chunk}"));
                commit();
                "#
            ))
            .unwrap();
        key
    }

    #[test]
    fn prover_delete_takes_the_task_its_receipt_and_the_index_entry() {
        let (_datadir, mut session) = seeded_session();
        let (chunk, acct) = seeded_tasks(&mut session);
        let unproved = unproved_chunk_task(&mut session, &chunk);

        assert_eq!(
            int(
                &mut session,
                &format!(r#"prover_delete("ChunkProverTaskSchema", "{unproved}")"#)
            ),
            1
        );
        session.db().abort();

        assert_eq!(
            int(
                &mut session,
                &format!(r#"prover_delete("ChunkProverTaskSchema", "{chunk}")"#)
            ),
            2
        );
        let ops = session.db().staged();
        assert_eq!(staged_of(&ops, "ChunkProverTaskSchema").len(), 1);
        assert_eq!(staged_of(&ops, "ChunkProofReceiptSchema").len(), 1);
        session.db().abort();

        assert_eq!(
            int(
                &mut session,
                &format!(r#"prover_delete("AcctProverTaskSchema", "{acct}")"#)
            ),
            3
        );
        let ops = session.db().staged();
        assert_eq!(staged_of(&ops, "AcctProverTaskSchema").len(), 1);
        assert_eq!(staged_of(&ops, "AcctProofReceiptSchema").len(), 1);
        assert_eq!(staged_of(&ops, "AcctProofIdIndexSchema").len(), 1);
        assert_eq!(int(&mut session, "commit()"), 3);
        assert_eq!(session.db().count("AcctProofIdIndexSchema").unwrap(), 0);
    }

    /// No ranges stage nothing, a range named twice is staged once, and a
    /// task key typed in uppercase still finds its receipt.
    #[test]
    fn prover_delete_ranges_takes_each_range_once() {
        let (_datadir, mut session) = seeded_session();
        let (chunk, _) = seeded_tasks(&mut session);
        let pair = format!("{}:{}", &chunk[4..68], &chunk[68..]);

        assert_eq!(
            int(
                &mut session,
                r#"prover_delete_ranges("ChunkProverTaskSchema", [])"#
            ),
            0
        );
        // The task and its receipt.
        assert_eq!(
            int(
                &mut session,
                &format!(r#"prover_delete_ranges("ChunkProverTaskSchema", ["{pair}", "{pair}"])"#)
            ),
            2
        );
        session.db().abort();

        let upper = chunk.to_uppercase();
        assert_eq!(
            int(
                &mut session,
                &format!(r#"prover_delete("ChunkProverTaskSchema", "{upper}")"#)
            ),
            2
        );
    }

    #[test]
    fn drop_chain_above_cuts_every_table_back_to_the_height() {
        let (_datadir, mut session) = seeded_session();
        // Seeded: heights 0..=2 finalized, witness diff at block number 6.
        let staged = int(&mut session, "drop_chain_above(1)");
        let ops = session.db().staged();
        assert_eq!(ops.len() as i64, staged);

        assert_eq!(staged_of(&ops, "ExecBlockSchema").len(), 1);
        assert_eq!(staged_of(&ops, "ExecBlockPayloadSchema").len(), 1);
        assert_eq!(staged_of(&ops, "ExecBlocksAtHeightSchema").len(), 1);
        assert_eq!(staged_of(&ops, "ExecBlockFinalizedSchema").len(), 1);
        // Block 2 carries no accessed state or witness in the seed; block 1
        // does and is kept.
        assert!(staged_of(&ops, "BlockAccessedStateSchema").is_empty());
        assert!(staged_of(&ops, "BlockWitnessSchema").is_empty());
        // The witness environment's diff at number 6 is above the height.
        assert_eq!(staged_of(&ops, "BlockHashByNumber").len(), 1);
        assert_eq!(staged_of(&ops, "BlockStateChangesSchema").len(), 1);
        // The genesis batch ends at block 0 and stays.
        assert!(staged_of(&ops, "BatchByIdxSchema").is_empty());
        // The seeded chunk ends at block 2, past every sealed batch, as a
        // chunk of the open batch would. It goes with its prover work.
        assert_eq!(staged_of(&ops, "ChunkByIdxSchema").len(), 1);
        assert_eq!(staged_of(&ops, "ChunkIdToIdxSchema").len(), 1);
        assert_eq!(staged_of(&ops, "ChunkProverTaskSchema").len(), 1);
        assert_eq!(staged_of(&ops, "ChunkProofReceiptSchema").len(), 1);
        assert!(staged_of(&ops, "AcctProverTaskSchema").is_empty());
        // The seeded epoch's account state points at block 2, which is gone,
        // so the epoch goes with it.
        assert_eq!(staged_of(&ops, "OLBlockAtEpochSchema").len(), 1);
        assert_eq!(staged_of(&ops, "AccountStateAtOLEpochSchema").len(), 1);
        // Node edits were staged first, prover edits last.
        assert_eq!(ops[0].env(), "node");
        assert_eq!(ops.last().unwrap().env(), "prover");

        assert_eq!(int(&mut session, "commit()"), staged);
        let db = session.db();
        assert_eq!(db.count("ExecBlockSchema").unwrap(), 2);
        assert_eq!(db.count("ExecBlocksAtHeightSchema").unwrap(), 2);
        assert_eq!(db.count("ExecBlockFinalizedSchema").unwrap(), 2);
        assert_eq!(db.count("BlockHashByNumber").unwrap(), 0);
        assert_eq!(db.count("BlockStateChangesSchema").unwrap(), 0);
        assert_eq!(db.count("BlockAccessedStateSchema").unwrap(), 1);
        assert_eq!(db.count("OLBlockAtEpochSchema").unwrap(), 0);
        assert_eq!(db.count("AccountStateAtOLEpochSchema").unwrap(), 0);
        assert_eq!(db.count("ChunkByIdxSchema").unwrap(), 0);
        assert_eq!(db.count("ChunkProverTaskSchema").unwrap(), 0);
        assert_eq!(db.count("AcctProverTaskSchema").unwrap(), 1);

        // Nothing above the tip: nothing staged.
        assert_eq!(int(&mut session, "drop_chain_above(1)"), 0);
        assert_eq!(int(&mut session, "drop_chain_above(99)"), 0);
    }

    /// The witness environment is trimmed by its own top: with the chain
    /// already at the height and the witness ahead of it, only the witness
    /// rows go.
    #[test]
    fn drop_chain_above_trims_a_witness_that_ran_ahead_of_the_chain_tip() {
        let (_datadir, mut session) = seeded_session();
        // Seeded: chain tip 2, witness diff at block number 6.
        assert_eq!(int(&mut session, "drop_chain_above(2)"), 2);
        let ops = session.db().staged();
        assert!(ops.iter().all(|op| op.env() == "witness"), "{ops:?}");
        assert_eq!(staged_of(&ops, "BlockHashByNumber").len(), 1);
        assert_eq!(staged_of(&ops, "BlockStateChangesSchema").len(), 1);
        assert_eq!(int(&mut session, "commit()"), 2);
        assert_eq!(session.db().count("ExecBlockSchema").unwrap(), 3);
        assert_eq!(session.db().count("BlockHashByNumber").unwrap(), 0);
        assert_eq!(int(&mut session, "drop_chain_above(2)"), 0);
    }

    /// A drop on a full node's datadir, which has no witness environment,
    /// cuts the node chain and skips the witness step rather than failing.
    #[test]
    fn drop_chain_above_on_a_full_node_cuts_only_the_node_chain() {
        let (_datadir, mut session) = full_node_session();
        // Two blocks at height 2, each with its payload, and the height entry.
        assert_eq!(int(&mut session, "drop_chain_above(1)"), 5);
        let ops = session.db().staged();
        assert!(ops.iter().all(|op| op.env() == "node"), "{ops:?}");
        assert_eq!(staged_of(&ops, "ExecBlockSchema").len(), 2);
        assert_eq!(staged_of(&ops, "ExecBlocksAtHeightSchema").len(), 1);
    }

    /// The hash of the seeded block whose bytes are all `seed`, as OL
    /// reports it.
    fn block_hash(seed: u8) -> String {
        format!("0x{}", hex::encode([seed; 32]))
    }

    #[test]
    fn drop_chain_above_a_hash_drops_what_its_height_would() {
        let (_datadir, mut session) = seeded_session();
        // Block 1 of the seeded chain is all 0x02.
        let by_hash = int(
            &mut session,
            &format!(r#"drop_chain_above("{}")"#, block_hash(2)),
        );
        let ops_by_hash = session.db().staged();
        session.db().abort();

        let by_height = int(&mut session, "drop_chain_above(1)");
        assert_eq!(by_hash, by_height);
        assert_eq!(ops_by_hash, session.db().staged());
    }

    #[test]
    fn drop_chain_above_refuses_a_hash_that_cannot_be_the_tip() {
        let (_datadir, mut session) = seeded_session();
        let refusal = |session: &mut Session, hash: &str| {
            let err = session
                .eval(&format!(r#"drop_chain_above("{hash}")"#))
                .unwrap_err()
                .to_string();
            assert!(session.db().staged().is_empty(), "staged on refusal");
            err
        };

        let err = refusal(&mut session, &block_hash(0xee));
        assert!(err.contains("no exec block"), "{err}");

        // A copy of block 1 under another hash sits at a finalized height
        // without being the finalized block there.
        let copy = "ef".repeat(32);
        let _ = session
            .eval(&format!(
                r#"
                put("ExecBlockSchema", "{copy}", edit("ExecBlockSchema", "{}"));
                commit();
                "#,
                block_hash(2)
            ))
            .unwrap();
        let err = refusal(&mut session, &copy);
        assert!(err.contains("not on the finalized chain"), "{err}");

        // On the full node, height 2 is unfinalized and holds two blocks.
        let (_datadir, mut session) = full_node_session();
        let err = refusal(&mut session, &block_hash(3));
        assert!(err.contains("shares height 2"), "{err}");
        // Block 1 has its height to itself, and both blocks above it go.
        assert_eq!(
            int(
                &mut session,
                &format!(r#"drop_chain_above("{}")"#, block_hash(2))
            ),
            5
        );
        session.db().abort();

        // A block the height index does not list is refused by name, not
        // with an error about a missing property.
        let _ = session
            .eval(r#"del("ExecBlocksAtHeightSchema", "2"); commit();"#)
            .unwrap();
        let err = refusal(&mut session, &block_hash(3));
        assert!(err.contains("height index does not list"), "{err}");
    }

    /// A height typed as a string is read as a hash and refused, and
    /// anything that is neither a string nor an integer is refused by type.
    #[test]
    fn drop_chain_above_takes_only_a_hash_or_a_height() {
        let (_datadir, mut session) = seeded_session();
        for (target, expected) in [
            (r#""1""#, "pass a height as a number"),
            ("1.5", "takes a block hash or a height"),
        ] {
            let err = session
                .eval(&format!("drop_chain_above({target})"))
                .unwrap_err()
                .to_string();
            assert!(err.contains(expected), "{target}: {err}");
            assert!(session.db().staged().is_empty(), "{target} staged");
        }
    }

    /// Rolled back to the end of its last sealed batch, the tip moves to the
    /// next version, one version at a time, once.
    #[test]
    fn set_next_spec_version_moves_the_rolled_back_tip_one_version_on() {
        let (_datadir, mut session) = seeded_session();
        // The genesis batch ends at block 0, which is all 0x01.
        let tip = block_hash(1);
        let _ = session
            .eval(&format!(r#"drop_chain_above("{tip}"); commit();"#))
            .unwrap();
        let next_version = format!(r#"get("ExecBlockSchema", "{tip}").value.next_spec_version"#);
        assert_eq!(int(&mut session, &next_version), 0);

        let err = session
            .eval(&format!(r#"set_next_spec_version("{tip}", 2)"#))
            .unwrap_err()
            .to_string();
        assert!(err.contains("can only move to 1"), "{err}");
        assert!(session.db().staged().is_empty());

        assert_eq!(
            int(
                &mut session,
                &format!(r#"set_next_spec_version("{tip}", 1)"#)
            ),
            1
        );
        let ops = session.db().staged();
        assert_eq!(ops.len(), 1);
        assert_eq!(staged_of(&ops, "ExecBlockSchema").len(), 1);
        assert_eq!(int(&mut session, "commit()"), 1);
        assert_eq!(int(&mut session, &next_version), 1);

        // A second run fails rather than skipping a version.
        let err = session
            .eval(&format!(r#"set_next_spec_version("{tip}", 1)"#))
            .unwrap_err()
            .to_string();
        assert!(err.contains("can only move to 2"), "{err}");
    }

    /// The builders continue from the tip and from the last sealed batch's
    /// last block, so any other block is refused, and so is a full node,
    /// which seals no batches.
    #[test]
    fn set_next_spec_version_refuses_a_block_the_builders_do_not_continue_from() {
        let refusal = |session: &mut Session, args: &str| {
            let err = session
                .eval(&format!("set_next_spec_version({args})"))
                .unwrap_err()
                .to_string();
            assert!(session.db().staged().is_empty(), "staged on refusal");
            err
        };

        let (_datadir, mut session) = seeded_session();
        // Block 2, all 0x03, is the tip, but the genesis batch ends at block 0.
        let err = refusal(&mut session, &format!(r#""{}", 1"#, block_hash(3)));
        assert!(err.contains("does not end the last sealed batch"), "{err}");
        let err = refusal(&mut session, &format!(r#""{}", 1"#, block_hash(2)));
        assert!(err.contains("is not the tip"), "{err}");
        let err = refusal(&mut session, &format!(r#""{}", "v1""#, block_hash(3)));
        assert!(err.contains("as a number"), "{err}");

        let (_datadir, mut session) = full_node_session();
        let _ = session.eval("drop_chain_above(1); commit();").unwrap();
        let err = refusal(&mut session, &format!(r#""{}", 1"#, block_hash(2)));
        assert!(err.contains("no sealed batch"), "{err}");
    }

    #[test]
    fn revert_batches_from_takes_batches_indexes_chunks_and_prover_work() {
        let (_datadir, mut session) = seeded_session();
        let staged = int(&mut session, "revert_batches_from(0)");
        let ops = session.db().staged();
        assert_eq!(staged_of(&ops, "BatchByIdxSchema").len(), 1);
        assert_eq!(staged_of(&ops, "BatchIdToIdxSchema").len(), 1);
        assert_eq!(staged_of(&ops, "BatchChunksSchema").len(), 1);
        assert_eq!(staged_of(&ops, "ChunkByIdxSchema").len(), 1);
        assert_eq!(staged_of(&ops, "ChunkIdToIdxSchema").len(), 1);
        // The seeded tasks and receipts are over the seeded chunk and batch.
        assert_eq!(staged_of(&ops, "ChunkProverTaskSchema").len(), 1);
        assert_eq!(staged_of(&ops, "ChunkProofReceiptSchema").len(), 1);
        assert_eq!(staged_of(&ops, "AcctProverTaskSchema").len(), 1);
        assert_eq!(staged_of(&ops, "AcctProofReceiptSchema").len(), 1);
        assert_eq!(staged_of(&ops, "AcctProofIdIndexSchema").len(), 1);
        assert_eq!(staged, 10);
        // Node edits were staged before prover edits.
        assert_eq!(ops[0].env(), "node");
        assert_eq!(ops.last().unwrap().env(), "prover");

        assert_eq!(int(&mut session, "commit()"), 10);
        for table in [
            "BatchByIdxSchema",
            "BatchIdToIdxSchema",
            "BatchChunksSchema",
            "ChunkByIdxSchema",
            "ChunkIdToIdxSchema",
            "ChunkProverTaskSchema",
            "ChunkProofReceiptSchema",
            "AcctProverTaskSchema",
            "AcctProofReceiptSchema",
            "AcctProofIdIndexSchema",
        ] {
            assert_eq!(session.db().count(table).unwrap(), 0, "{table}");
        }
        assert!(
            session
                .eval("revert_batches_from(0)")
                .unwrap()
                .as_int()
                .unwrap()
                == 0
        );
        let _: Dynamic = session.eval("()").unwrap();
    }
}
