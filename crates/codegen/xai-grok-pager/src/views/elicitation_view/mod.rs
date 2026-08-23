//! MCP elicitation card (`x.ai/mcp/elicit`): form and URL elicitation with
//! the same lifecycle as the other HITL cards (permission / question / plan
//! approval). State transitions live in [`state`], painting in [`render`].

mod render;
mod state;
#[cfg(test)]
mod tests;

pub use render::{
    ElicitHit, elicitation_view_height, elicitation_view_height_with_locale,
    render_elicitation_view, render_elicitation_view_with_locale,
};
pub use state::{
    ElicitResponseTx, ElicitationActionFocus, ElicitationFocus, ElicitationStage,
    ElicitationViewState, FieldValueUi, FormFieldUi, FormStage, UrlConsentStage, UrlDisplay,
    UrlWaitingStage,
};
