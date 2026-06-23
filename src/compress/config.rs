use serde::Deserialize;
use std::collections::HashMap;

#[derive(Debug, Deserialize, Default, Clone)]
pub struct CompressConfig {
    #[serde(default)]
    pub providers: HashMap<String, CompressProviderConfig>,
}

#[derive(Debug, Deserialize, Default, Clone)]
pub struct CompressProviderConfig {
    /// Hard cap on characters per tool result.
    /// Larger results are truncated with head+tail extraction. 0 = no limit.
    pub max_tool_chars: Option<usize>,
    /// Run SmartCrusher on JSON array tool results.
    pub smart_crusher: Option<bool>,
    /// Bias multiplier for adaptive sizing (>1 = keep more, <1 = compress
    /// harder). Default 1.0.
    pub bias: Option<f64>,
}
