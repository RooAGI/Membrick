use std::time::Duration;

use membrick::{
    AddOptions as CoreAddOptions, GetOptions, ListOptions as CoreListOptions,
    MemoryOptions as CoreMemoryOptions, SearchOptions as CoreSearchOptions,
    UpdateOptions as CoreUpdateOptions,
};
use napi::bindgen_prelude::*;
use napi_derive::napi;
use serde::{de::DeserializeOwned, Serialize};
use serde_json::Value;

#[napi(object)]
pub struct MemoryOptions {
    pub path: Option<String>,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub request_timeout_ms: Option<u32>,
    pub resident_executable: Option<String>,
}

#[napi(object)]
pub struct AddOptions {
    pub request_id: Option<String>,
    pub session_id: Option<String>,
    pub wait_for_visibility: Option<bool>,
}

#[napi(object)]
pub struct SearchOptions {
    pub top_k: Option<u32>,
    pub session_id: Option<String>,
    pub filters: Option<Value>,
    pub scope: Option<String>,
    pub reference_date: Option<String>,
    pub options: Option<Vec<String>>,
}

#[napi(object)]
pub struct ListOptions {
    pub session_id: Option<String>,
    pub limit: Option<u32>,
    pub cursor: Option<String>,
    pub include_inactive: Option<bool>,
}

#[napi(object)]
pub struct UpdateOptions {
    pub role: Option<String>,
    pub timestamp: Option<f64>,
    pub expires_at_ms: Option<f64>,
}

#[napi(js_name = "Memory")]
pub struct Memory {
    inner: std::sync::Arc<membrick::Memory>,
}

#[napi]
impl Memory {
    #[napi(constructor)]
    pub fn new(options: Option<MemoryOptions>) -> Result<Self> {
        let options = options.unwrap_or(MemoryOptions {
            path: None,
            base_url: None,
            api_key: None,
            request_timeout_ms: None,
            resident_executable: None,
        });
        if options.path.is_some() && options.base_url.is_some() {
            return Err(Error::from_reason(
                "[MEMBRICK_ERROR code=CONFIGURATION] path and baseUrl cannot both be set",
            ));
        }
        let client = match options.base_url {
            Some(base_url) => membrick::Memory::remote(
                base_url,
                CoreMemoryOptions {
                    api_key: options.api_key,
                    request_timeout: Duration::from_millis(
                        options.request_timeout_ms.unwrap_or(30_000) as u64,
                    ),
                    resident_executable: options.resident_executable.map(std::path::PathBuf::from),
                },
            ),
            None => match options.path {
                None => return Err(Error::from_reason("[MEMBRICK_ERROR code=CONFIGURATION] provide path for a project connection or baseUrl for a remote connection")),
                Some(path) => {
                    membrick::Memory::with_options(path, CoreMemoryOptions {
                        api_key: options.api_key,
                        request_timeout: Duration::from_millis(
                            options.request_timeout_ms.unwrap_or(30_000) as u64,
                        ),
                        resident_executable: options.resident_executable.map(std::path::PathBuf::from),
                    })
                }
            },
        }
        .map_err(memory_client_error)?;
        Ok(Self {
            inner: std::sync::Arc::new(client),
        })
    }

    #[napi(factory)]
    pub fn init(options: MemoryOptions) -> Result<Self> {
        let path = options.path.ok_or_else(|| {
            Error::from_reason("[MEMBRICK_ERROR code=CONFIGURATION] Memory.init requires path")
        })?;
        if options.base_url.is_some() {
            return Err(Error::from_reason(
                "[MEMBRICK_ERROR code=CONFIGURATION] Memory.init cannot be used with baseUrl",
            ));
        }
        let client = membrick::Memory::init_with_options(
            path,
            CoreMemoryOptions {
                api_key: options.api_key,
                request_timeout: Duration::from_millis(
                    options.request_timeout_ms.unwrap_or(30_000) as u64,
                ),
                resident_executable: options.resident_executable.map(std::path::PathBuf::from),
            },
        )
        .map_err(memory_client_error)?;
        Ok(Self {
            inner: std::sync::Arc::new(client),
        })
    }

    #[napi]
    pub async fn add(
        &self,
        messages: Vec<Value>,
        user_id: String,
        options: Option<AddOptions>,
    ) -> Result<Value> {
        let options = options.unwrap_or(AddOptions {
            request_id: None,
            session_id: None,
            wait_for_visibility: None,
        });
        let messages = messages
            .into_iter()
            .map(from_value)
            .collect::<Result<Vec<membrick::Message>>>()?;
        let client = self.inner.clone();
        spawn_blocking(move || {
            let receipt = client
                .add(
                    messages,
                    &user_id,
                    CoreAddOptions {
                        request_id: options.request_id,
                        session_id: options.session_id,
                        wait_for_visibility: options.wait_for_visibility.unwrap_or(false),
                    },
                )
                .map_err(memory_client_error)?;
            to_value(receipt)
        })
        .await
    }

    #[napi]
    pub async fn add_transaction(&self, requests: Vec<Value>) -> Result<Vec<Value>> {
        let requests = requests
            .into_iter()
            .map(from_value::<membrick::AddRequestWithMetadata>)
            .collect::<Result<Vec<_>>>()?;
        let client = self.inner.clone();
        spawn_blocking(move || {
            let responses = client
                .add_transaction(requests)
                .map_err(memory_client_error)?;
            to_values(responses)
        })
        .await
    }

    #[napi]
    pub async fn search(
        &self,
        query: String,
        user_id: String,
        options: Option<SearchOptions>,
    ) -> Result<Vec<Value>> {
        let options = options.unwrap_or(SearchOptions {
            top_k: None,
            session_id: None,
            filters: None,
            scope: None,
            reference_date: None,
            options: None,
        });
        let filters = options
            .filters
            .filter(|value| !value.is_null())
            .map(from_value::<std::collections::BTreeMap<String, String>>)
            .transpose()?;
        let client = self.inner.clone();
        spawn_blocking(move || {
            let results = client
                .search(
                    &query,
                    &user_id,
                    CoreSearchOptions {
                        top_k: options.top_k.unwrap_or(10) as usize,
                        session_id: options.session_id,
                        filters,
                        scope: options.scope,
                        reference_date: options.reference_date,
                        options: options.options,
                    },
                )
                .map_err(memory_client_error)?;
            to_values(results)
        })
        .await
    }

    #[napi]
    pub async fn get(
        &self,
        memory_id: String,
        user_id: String,
        include_inactive: Option<bool>,
    ) -> Result<Option<Value>> {
        let client = self.inner.clone();
        spawn_blocking(move || {
            client
                .get(
                    &memory_id,
                    &user_id,
                    GetOptions {
                        include_inactive: include_inactive.unwrap_or(false),
                    },
                )
                .map_err(memory_client_error)?
                .map(to_value)
                .transpose()
        })
        .await
    }

    #[napi]
    pub async fn list(&self, user_id: String, options: Option<ListOptions>) -> Result<Value> {
        let options = options.unwrap_or(ListOptions {
            session_id: None,
            limit: None,
            cursor: None,
            include_inactive: None,
        });
        let client = self.inner.clone();
        spawn_blocking(move || {
            let result = client
                .list(
                    &user_id,
                    CoreListOptions {
                        session_id: options.session_id,
                        limit: options.limit.unwrap_or(100) as usize,
                        cursor: options.cursor,
                        include_inactive: options.include_inactive.unwrap_or(false),
                    },
                )
                .map_err(memory_client_error)?;
            to_value(result)
        })
        .await
    }

    #[napi]
    pub async fn update(
        &self,
        memory_id: String,
        user_id: String,
        content: String,
        options: Option<UpdateOptions>,
    ) -> Result<Option<Value>> {
        let options = options.unwrap_or(UpdateOptions {
            role: None,
            timestamp: None,
            expires_at_ms: None,
        });
        let timestamp = options
            .timestamp
            .map(|v| safe_integer(v, "timestamp"))
            .transpose()?;
        let expires_at_ms = options
            .expires_at_ms
            .map(|v| safe_integer(v, "expires_at_ms"))
            .transpose()?
            .map(|v| {
                u64::try_from(v)
                    .map_err(|_| Error::from_reason("expires_at_ms must be non-negative"))
            })
            .transpose()?;
        let client = self.inner.clone();
        spawn_blocking(move || {
            client
                .update(
                    &memory_id,
                    &user_id,
                    &content,
                    CoreUpdateOptions {
                        role: options.role,
                        timestamp,
                        expires_at_ms,
                    },
                )
                .map_err(memory_client_error)?
                .map(to_value)
                .transpose()
        })
        .await
    }

    #[napi]
    pub async fn delete(&self, memory_id: String, user_id: String) -> Result<bool> {
        let client = self.inner.clone();
        spawn_blocking(move || {
            client
                .delete(&memory_id, &user_id)
                .map_err(memory_client_error)
        })
        .await
    }

    #[napi]
    pub async fn refresh(&self) -> Result<()> {
        let client = self.inner.clone();
        spawn_blocking(move || client.refresh().map_err(memory_client_error)).await
    }

    #[napi]
    pub fn close(&self) {
        self.inner.close();
    }

    #[napi]
    pub fn session(&self, user_id: String, session_id: String) -> MemorySession {
        MemorySession {
            inner: std::sync::Arc::new((*self.inner).clone()),
            user_id,
            session_id,
        }
    }
}

#[napi(object)]
pub struct SessionSearchOptions {
    pub top_k: Option<u32>,
    pub filters: Option<Value>,
    pub scope: Option<String>,
    pub reference_date: Option<String>,
    pub options: Option<Vec<String>>,
}

#[napi]
pub struct MemorySession {
    inner: std::sync::Arc<membrick::Memory>,
    user_id: String,
    session_id: String,
}

#[napi]
impl MemorySession {
    #[napi(getter)]
    pub fn user_id(&self) -> String {
        self.user_id.clone()
    }
    #[napi(getter)]
    pub fn session_id(&self) -> String {
        self.session_id.clone()
    }
    #[napi]
    pub fn close(&self) {
        self.inner.close();
    }
    #[napi]
    pub async fn add(&self, messages: Vec<Value>, options: Option<AddOptions>) -> Result<Value> {
        let options = options.unwrap_or(AddOptions {
            request_id: None,
            session_id: None,
            wait_for_visibility: None,
        });
        let messages = messages
            .into_iter()
            .map(from_value)
            .collect::<Result<Vec<membrick::Message>>>()?;
        let client = self.inner.clone();
        let user_id = self.user_id.clone();
        let session_id = self.session_id.clone();
        spawn_blocking(move || {
            let receipt = client
                .add(
                    messages,
                    &user_id,
                    CoreAddOptions {
                        request_id: options.request_id,
                        session_id: Some(session_id),
                        wait_for_visibility: options.wait_for_visibility.unwrap_or(false),
                    },
                )
                .map_err(memory_client_error)?;
            to_value(receipt)
        })
        .await
    }
    #[napi]
    pub async fn search(
        &self,
        query: String,
        options: Option<SessionSearchOptions>,
    ) -> Result<Vec<Value>> {
        let options = options.unwrap_or(SessionSearchOptions {
            top_k: None,
            filters: None,
            scope: None,
            reference_date: None,
            options: None,
        });
        let filters = options
            .filters
            .filter(|value| !value.is_null())
            .map(from_value::<std::collections::BTreeMap<String, String>>)
            .transpose()?;
        let client = self.inner.clone();
        let user_id = self.user_id.clone();
        let session_id = self.session_id.clone();
        spawn_blocking(move || {
            let results = client
                .search(
                    &query,
                    &user_id,
                    CoreSearchOptions {
                        top_k: options.top_k.unwrap_or(10) as usize,
                        session_id: Some(session_id),
                        filters,
                        scope: options.scope,
                        reference_date: options.reference_date,
                        options: options.options,
                    },
                )
                .map_err(memory_client_error)?;
            to_values(results)
        })
        .await
    }
}

fn from_value<T: DeserializeOwned>(value: Value) -> Result<T> {
    serde_json::from_value(value).map_err(runtime_error)
}

fn to_value<T: Serialize>(value: T) -> Result<Value> {
    serde_json::to_value(value).map_err(runtime_error)
}

fn to_values<T: Serialize>(values: Vec<T>) -> Result<Vec<Value>> {
    values.into_iter().map(to_value).collect()
}

fn safe_integer(value: f64, field: &str) -> Result<i64> {
    const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;
    if !value.is_finite() || value.fract() != 0.0 || value.abs() > MAX_SAFE_INTEGER {
        return Err(Error::from_reason(format!(
            "{field} must be a safe integer"
        )));
    }
    Ok(value as i64)
}

fn runtime_error(error: impl std::fmt::Display) -> Error {
    Error::from_reason(error.to_string())
}

async fn spawn_blocking<T, F>(operation: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(runtime_error)?
}

fn memory_client_error(error: membrick::MemoryError) -> Error {
    match error {
        membrick::MemoryError::Rejected { status, message } => Error::from_reason(format!(
            "[MEMBRICK_ERROR code=REJECTED status={status}] request rejected ({status}): {message}"
        )),
        membrick::MemoryError::Configuration { message } => Error::from_reason(format!(
            "[MEMBRICK_ERROR code=CONFIGURATION] invalid client configuration: {message}"
        )),
        membrick::MemoryError::Unavailable { message } => Error::from_reason(format!(
            "[MEMBRICK_ERROR code=UNAVAILABLE] memory service unavailable: {message}"
        )),
        membrick::MemoryError::Uncertain { message } => Error::from_reason(format!(
            "[MEMBRICK_ERROR code=UNCERTAIN] memory request outcome uncertain: {message}"
        )),
    }
}

#[napi]
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
