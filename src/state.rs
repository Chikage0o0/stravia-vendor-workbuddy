use serde_json::{Map, Value};
use stravia_vendor_common::common;
use stravia_vendor_sdk::{
    ErrorKind, GuestHost, PluginError,
    state::{self, Scope},
};

const PAID_WINDOWS_KEY: &str = "workbuddy_paid_windows";

pub(crate) fn read(host: &GuestHost) -> Result<Map<String, Value>, PluginError> {
    let Some(bytes) = host.read_private_state()?.filter(|bytes| !bytes.is_empty()) else {
        return Ok(Map::new());
    };
    serde_json::from_slice(&bytes).map_err(|_| {
        common::plugin_error(
            ErrorKind::Invalid,
            "stored WorkBuddy private state is malformed",
        )
    })
}

pub(crate) fn write(host: &GuestHost, root: Map<String, Value>) -> Result<(), PluginError> {
    if root.is_empty() {
        return host.write_private_state(b"");
    }
    let bytes = serde_json::to_vec(&root).map_err(|_| {
        common::plugin_error(
            ErrorKind::Invalid,
            "WorkBuddy private state could not be encoded",
        )
    })?;
    host.write_private_state(&bytes)
}

// Pending authentication remains in private state, which is available before
// provider admission. Paid windows use a separate provider-scoped atomic cell.
pub(crate) fn read_paid_windows() -> Result<Map<String, Value>, PluginError> {
    let cell = state::open(Scope::Provider, PAID_WINDOWS_KEY)?;
    load_paid_windows(&cell).map(|(_, root)| root)
}

pub(crate) fn write_paid_window(key: String, reset: i64) -> Result<(), PluginError> {
    let cell = state::open(Scope::Provider, PAID_WINDOWS_KEY)?;
    for _ in 0..32 {
        let (revision, mut root) = load_paid_windows(&cell)?;
        let windows = root
            .entry(PAID_WINDOWS_KEY)
            .or_insert_with(|| Value::Object(Map::new()))
            .as_object_mut()
            .filter(|windows| windows.values().all(|reset| reset.as_i64().is_some()))
            .ok_or_else(malformed_paid_windows)?;
        if windows.get(&key).and_then(Value::as_i64) == Some(reset) {
            return Ok(());
        }
        windows.insert(key.clone(), Value::from(reset));
        let bytes = serde_json::to_vec(&root).map_err(|_| {
            common::plugin_error(
                ErrorKind::Invalid,
                "WorkBuddy paid windows could not be encoded",
            )
        })?;
        match cell.compare_exchange(revision, Some(&bytes)) {
            Ok(()) => return Ok(()),
            Err(state::Error::Conflict) => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Err(state::Error::Conflict.into())
}

fn load_paid_windows(cell: &state::Cell) -> Result<(u64, Map<String, Value>), PluginError> {
    let snapshot = cell.load()?;
    let root = match snapshot.payload {
        Some(bytes) => serde_json::from_slice(&bytes).map_err(|_| malformed_paid_windows())?,
        None => Map::new(),
    };
    Ok((snapshot.revision, root))
}

fn malformed_paid_windows() -> PluginError {
    common::plugin_error(
        ErrorKind::Invalid,
        "stored WorkBuddy paid windows are malformed",
    )
}
