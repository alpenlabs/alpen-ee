//! EVM block implementation.

use alpen_acct_types::ExecBlock;
use strata_codec::impl_type_flat_struct;

use super::{EvmBlockBody, EvmHeader};

impl_type_flat_struct! {
    /// Full EVM block containing header and body.
    ///
    /// Represents a complete Ethereum block with header metadata and transaction body.
    /// This is the top-level block type used in the ExecutionEnvironment.
    #[derive(Clone, Debug)]
    pub struct EvmBlock {
        header: EvmHeader,
        body: EvmBlockBody,
    }
}

impl ExecBlock for EvmBlock {
    type Header = EvmHeader;
    type Body = EvmBlockBody;

    fn from_parts(header: Self::Header, body: Self::Body) -> Self {
        Self { header, body }
    }

    fn get_header(&self) -> &Self::Header {
        &self.header
    }

    fn get_body(&self) -> &Self::Body {
        &self.body
    }
}
