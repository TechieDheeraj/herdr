//! Persistent shell history identity, independent of pane addresses and PTYs.
use std::io;

pub(crate) const ENV_VAR: &str = "HERDR_SHELL_HISTORY_ID";

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "String", into = "String")]
pub(crate) struct ShellHistoryId(String);

impl ShellHistoryId {
    pub(crate) fn new() -> io::Result<Self> {
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes).map_err(io::Error::other)?;
        Ok(Self(
            bytes.iter().map(|byte| format!("{byte:02x}")).collect(),
        ))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ShellHistoryId {
    type Error = &'static str;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value.len() == 32 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            Ok(Self(value))
        } else {
            Err("invalid shell history identity")
        }
    }
}

impl From<ShellHistoryId> for String {
    fn from(value: ShellHistoryId) -> Self {
        value.0
    }
}

/// An invalid optional identity must not make the whole session unreadable.
pub(crate) fn read_optional<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<ShellHistoryId>, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(serde_json::from_value(value).ok())
}

use serde::Deserialize;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_is_unique_and_cannot_contain_a_path() {
        let first = ShellHistoryId::new().unwrap();
        let second = ShellHistoryId::new().unwrap();
        assert_ne!(first, second);
        assert_eq!(ShellHistoryId::try_from(first.0.clone()).unwrap(), first);
        for invalid in ["../../history", "", "/tmp/history", "w1:p1"] {
            assert!(ShellHistoryId::try_from(invalid.to_owned()).is_err());
        }
    }
}
