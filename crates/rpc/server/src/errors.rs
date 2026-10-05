use jsonrpsee::types::{
    error::{INTERNAL_ERROR_CODE, INVALID_PARAMS_CODE, METHOD_NOT_FOUND_CODE},
    ErrorObjectOwned,
};

/// Creates an RPC error for internal failures.
pub(crate) fn internal_error(msg: impl Into<String>) -> ErrorObjectOwned {
    ErrorObjectOwned::owned(INTERNAL_ERROR_CODE, msg.into(), None::<()>)
}

/// Creates an RPC error for missing block hash input.
pub(crate) fn block_not_found_error() -> ErrorObjectOwned {
    ErrorObjectOwned::owned(INVALID_PARAMS_CODE, "block not found", None::<()>)
}

/// Creates an RPC error for invalid parameters.
pub(crate) fn invalid_params_error(msg: impl Into<String>) -> ErrorObjectOwned {
    ErrorObjectOwned::owned(INVALID_PARAMS_CODE, msg.into(), None::<()>)
}

/// Creates an RPC error for sequencer-only methods called on a full node.
pub(crate) fn not_sequencer_error() -> ErrorObjectOwned {
    ErrorObjectOwned::owned(
        METHOD_NOT_FOUND_CODE,
        "block production control is only available on sequencer nodes",
        None::<()>,
    )
}
