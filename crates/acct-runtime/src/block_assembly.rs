//! Helpers for working with ee account during block assembly.

use alpen_acct_types::EeAccountState;
use strata_acct_types::MessageEntry;
use strata_snark_acct_runtime::InputMessage;

use crate::{ee_program::process_input_message, errors::AcctProgramResult};

/// Applies state changes from a list of messages.
pub fn apply_input_messages(
    astate: &mut EeAccountState,
    msgs: &[MessageEntry],
) -> AcctProgramResult<()> {
    for entry in msgs.iter() {
        let input_msg = InputMessage::from_msg_entry(entry);

        process_input_message(astate, &input_msg)?;
    }

    Ok(())
}
