//! The recipes shipped with the binary.
//!
//! A recipe is a rhai function in one of the files under `recipes/`,
//! embedded at build time and defined in every session before the first
//! input, so it is versioned and reviewed with the binary. A recipe stages
//! its edits and returns how many; it never commits. `.staged` is the
//! preview and `commit()` the act, which is the dry-run default the console
//! is built around. `.recipes` lists them with their doc comments.
//!
//! Recipes that touch two environments stage the authoritative one first,
//! because `commit()` applies environments in first-staged order and cannot
//! be atomic across them; the witness side is a regenerable cache.

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

    use alpen_ee_database::{
        console::{ConsoleDb, StagedOp},
        test_db::TempDatadir,
    };
    use rhai::{Dynamic, Map};

    use super::*;

    /// A read-write session over a datadir with a row in every table. The
    /// datadir is returned with it and must outlive the session.
    fn seeded_session() -> (TempDatadir, Session) {
        let datadir = TempDatadir::seeded();
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
        assert_eq!(prover.get("Pending").unwrap().as_int().unwrap(), 1);

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

    #[test]
    fn prover_reset_and_abandon_stage_a_status_change_and_nothing_else() {
        let (_datadir, mut session) = seeded_session();
        assert_eq!(int(&mut session, r#"prover_reset("010203")"#), 1);
        let staged = session.db().staged();
        assert_eq!(staged.len(), 1);
        assert!(matches!(&staged[0], StagedOp::Put { table, record, .. }
            if *table == "ProverTaskSchema" && record.value.get("status").unwrap().to_string() == "Pending"));
        session.db().abort();

        assert_eq!(
            int(
                &mut session,
                r#"prover_abandon("010203", "operator gave up")"#
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

    /// Builds a task at a receipt-shaped key by copying the seeded rows:
    /// a chunk task over a chunk receipt, a batch task over an account
    /// receipt whose proof-id index entry the seed also wrote.
    fn seed_receipted_tasks(session: &mut Session) -> (String, String) {
        let hashes = "04".repeat(64);
        let chunk_task = format!("000163{hashes}");
        let chunk_receipt = format!("63{hashes}");
        // The seeded account receipt sits at the genesis batch's pair key and
        // the seeded index entry points at that same pair.
        let pair = session
            .eval(r#"first("AcctProofReceiptSchema").key"#)
            .unwrap()
            .to_string();
        let (prev, last) = pair.split_once(':').unwrap();
        let batch_task = format!("000161{prev}{last}");
        let _ = session
            .eval(&format!(
                r#"
                put("ChunkProofReceiptSchema", "{chunk_receipt}", edit("ChunkProofReceiptSchema", "040506"));
                put("ProverTaskSchema", "{chunk_task}", edit("ProverTaskSchema", "010203"));
                put("ProverTaskSchema", "{batch_task}", edit("ProverTaskSchema", "010203"));
                commit();
                "#
            ))
            .unwrap();
        (chunk_task, batch_task)
    }

    #[test]
    fn prover_task_finds_the_receipt_behind_a_task_key() {
        let (_datadir, mut session) = seeded_session();
        let (chunk_task, batch_task) = seed_receipted_tasks(&mut session);

        let shown = map(&mut session, &format!(r#"prover_task("{chunk_task}")"#));
        assert!(shown.get("task").unwrap().is_map());
        assert!(
            shown.get("receipt").unwrap().is_map(),
            "chunk receipt not found"
        );
        let shown = map(&mut session, &format!(r#"prover_task("{batch_task}")"#));
        assert!(
            shown.get("receipt").unwrap().is_map(),
            "account receipt not found"
        );
        // A key of neither shape has no receipt and does not fail.
        let shown = map(&mut session, r#"prover_task("010203")"#);
        assert!(shown.get("receipt").unwrap().is_unit());
    }

    #[test]
    fn prover_delete_takes_the_task_its_receipt_and_the_index_entry() {
        let (_datadir, mut session) = seeded_session();
        let (chunk_task, batch_task) = seed_receipted_tasks(&mut session);

        assert_eq!(int(&mut session, r#"prover_delete("010203")"#), 1);
        session.db().abort();

        assert_eq!(
            int(&mut session, &format!(r#"prover_delete("{chunk_task}")"#)),
            2
        );
        let ops = session.db().staged();
        assert_eq!(staged_of(&ops, "ProverTaskSchema").len(), 1);
        assert_eq!(staged_of(&ops, "ChunkProofReceiptSchema").len(), 1);
        session.db().abort();

        assert_eq!(
            int(&mut session, &format!(r#"prover_delete("{batch_task}")"#)),
            3
        );
        let ops = session.db().staged();
        assert_eq!(staged_of(&ops, "ProverTaskSchema").len(), 1);
        assert_eq!(staged_of(&ops, "AcctProofReceiptSchema").len(), 1);
        assert_eq!(staged_of(&ops, "AcctProofIdIndexSchema").len(), 1);
        assert_eq!(int(&mut session, "commit()"), 3);
        assert_eq!(session.db().count("AcctProofIdIndexSchema").unwrap(), 0);
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
        // The seeded epoch's account state points at block 2, which is gone,
        // so the epoch goes with it.
        assert_eq!(staged_of(&ops, "OLBlockAtEpochSchema").len(), 1);
        assert_eq!(staged_of(&ops, "AccountStateAtOLEpochSchema").len(), 1);
        // Node edits were staged before witness edits.
        assert_eq!(ops[0].env(), "node");
        assert_eq!(ops.last().unwrap().env(), "witness");

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

    #[test]
    fn revert_batches_from_takes_batches_indexes_and_chunks() {
        let (_datadir, mut session) = seeded_session();
        let staged = int(&mut session, "revert_batches_from(0)");
        let ops = session.db().staged();
        assert_eq!(staged_of(&ops, "BatchByIdxSchema").len(), 1);
        assert_eq!(staged_of(&ops, "BatchIdToIdxSchema").len(), 1);
        assert_eq!(staged_of(&ops, "BatchChunksSchema").len(), 1);
        assert_eq!(staged_of(&ops, "ChunkByIdxSchema").len(), 1);
        assert_eq!(staged_of(&ops, "ChunkIdToIdxSchema").len(), 1);
        assert_eq!(staged, 5);

        assert_eq!(int(&mut session, "commit()"), 5);
        for table in [
            "BatchByIdxSchema",
            "BatchIdToIdxSchema",
            "BatchChunksSchema",
            "ChunkByIdxSchema",
            "ChunkIdToIdxSchema",
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
