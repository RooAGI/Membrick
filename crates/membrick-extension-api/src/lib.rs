//! Generic contract for compiled-in Membrick extensions.
//!
//! An extension crate describes the capabilities it provides and requires,
//! declares the tools it exposes, implements [`ExtensionHandler`], and submits
//! one [`ExtensionRegistration`] to [`EXTENSIONS`]. The final binary gathers
//! registrations from its linked dependency graph at link time. The host then
//! validates the complete set once and dispatches tools by [`ToolId`].
//!
//! This API does not load shared libraries or create a runtime plugin loader.
//! An extension must still be a dependency of the final binary to be linked.
//!
//! # Registration shape
//!
//! ```ignore
//! use membrick_extension_api::{
//!     distributed_slice, CapabilityId, ExtensionContext, ExtensionDescriptor, ExtensionError,
//!     ExtensionHandler, ExtensionId, ExtensionRegistration, ToolDefinition, ToolDescriptor,
//!     ToolId, EXTENSIONS,
//! };
//!
//! static TOOLS: &[ToolDescriptor] = &[
//!     ToolDescriptor {
//!         id: ToolId::new("example.echo"),
//!         description: "Echo a JSON value",
//!         input_schema: r#"{"type":"object"}"#,
//!     },
//!     ToolDescriptor {
//!         id: ToolId::new("example.echo-again"),
//!         description: "Echo another JSON value",
//!         input_schema: r#"{"type":"object"}"#,
//!     },
//! ];
//!
//! static DESCRIPTOR: ExtensionDescriptor = ExtensionDescriptor {
//!     id: ExtensionId::new("example.echo-extension"),
//!     provides: &[CapabilityId::new("example.echo")],
//!     requires: &[],
//!     tools: TOOLS,
//! };
//!
//! struct Echo;
//! impl ExtensionHandler for Echo {
//!     fn tool_definitions(&self) -> Vec<ToolDefinition> {
//!         // The second descriptor is automatically listed from `TOOLS`.
//!         vec![ToolDefinition {
//!             name: "example.echo".to_owned(),
//!             description: "Echo JSON, with a custom display description".to_owned(),
//!             input_schema: serde_json::json!({"type": "object"}),
//!         }]
//!     }
//!
//!     fn call(
//!         &self,
//!         context: &mut dyn ExtensionContext,
//!         _tool: ToolId,
//!         input: serde_json::Value,
//!     ) -> Result<serde_json::Value, ExtensionError> {
//!         // Attribution is host-supplied, never trusted from tool arguments.
//!         let _identity = context.invocation_identity();
//!         Ok(input)
//!     }
//! }
//! static HANDLER: Echo = Echo;
//!
//! #[distributed_slice(EXTENSIONS)]
//! static REGISTRATION: ExtensionRegistration = ExtensionRegistration {
//!     descriptor: &DESCRIPTOR,
//!     handler: &HANDLER,
//! };
//! ```
//!
//! If a handler does not override `tool_definitions`, the registry builds one
//! definition per descriptor tool from its ID, description, and JSON Schema.
//! Handler-provided definitions may customize those values, but cannot add or
//! duplicate tool IDs; omitted definitions are filled from the descriptor.

pub use linkme::distributed_slice;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::error::Error;
use std::fmt;
use std::sync::Mutex;

/// Provider-neutral MCP tool definition exported by an extension.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
}

/// Stable identifier for one extension crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ExtensionId(&'static str);

impl ExtensionId {
    /// Declare an extension identifier in a static descriptor.
    pub const fn new(value: &'static str) -> Self {
        Self(value)
    }

    pub const fn as_str(self) -> &'static str {
        self.0
    }
}

impl fmt::Display for ExtensionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

/// Stable identifier for a capability supplied by an extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CapabilityId(&'static str);

impl CapabilityId {
    /// Declare a capability identifier in a static extension descriptor.
    pub const fn new(value: &'static str) -> Self {
        Self(value)
    }

    pub const fn as_str(self) -> &'static str {
        self.0
    }
}

impl fmt::Display for CapabilityId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

/// Stable identifier for a tool exposed through a host transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ToolId(&'static str);

impl ToolId {
    /// Declare a tool identifier in a static extension descriptor.
    pub const fn new(value: &'static str) -> Self {
        Self(value)
    }

    pub const fn as_str(self) -> &'static str {
        self.0
    }
}

impl fmt::Display for ToolId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

/// Owned extension identity for runtime metadata.
///
/// Unlike [`ExtensionId`], this value can be created from a verified manifest
/// and does not borrow a static string. It is metadata only; it does not make
/// extension code executable or registered for dispatch.
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct OwnedExtensionId(String);

impl OwnedExtensionId {
    pub fn try_new(value: impl Into<String>) -> Result<Self, OwnedIdentifierError> {
        let value = value.into();
        validate_owned_identifier(&value, "extension")?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for OwnedExtensionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Owned tool identity for signed runtime declarations.
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct OwnedToolId(String);

impl OwnedToolId {
    pub fn try_new(value: impl Into<String>) -> Result<Self, OwnedIdentifierError> {
        let value = value.into();
        validate_owned_identifier(&value, "tool")?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for OwnedToolId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Owned capability identity used to validate dynamic metadata dependencies.
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct OwnedCapabilityId(String);

impl OwnedCapabilityId {
    pub fn try_new(value: impl Into<String>) -> Result<Self, OwnedIdentifierError> {
        let value = value.into();
        validate_owned_identifier(&value, "capability")?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for OwnedCapabilityId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

const MAX_OWNED_IDENTIFIER_BYTES: usize = 128;

fn validate_owned_identifier(value: &str, kind: &'static str) -> Result<(), OwnedIdentifierError> {
    let mut bytes = value.bytes();
    let valid_first = bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphanumeric());
    if value.is_empty()
        || value.len() > MAX_OWNED_IDENTIFIER_BYTES
        || !valid_first
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
        || !value
            .as_bytes()
            .last()
            .is_some_and(u8::is_ascii_alphanumeric)
    {
        return Err(OwnedIdentifierError { kind });
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OwnedIdentifierError {
    kind: &'static str,
}

impl fmt::Display for OwnedIdentifierError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "invalid {} identifier", self.kind)
    }
}

impl Error for OwnedIdentifierError {}

/// One signed, generic tool declaration. The schema is represented as JSON
/// data and must be an object; this type contains no executable handler.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OwnedToolDescriptor {
    pub id: OwnedToolId,
    pub description: String,
    #[serde(rename = "input_schema")]
    pub input_schema: Value,
}

impl OwnedToolDescriptor {
    pub fn new(
        id: impl Into<String>,
        description: impl Into<String>,
        input_schema: Value,
    ) -> Result<Self, OwnedIdentifierError> {
        Ok(Self {
            id: OwnedToolId::try_new(id)?,
            description: description.into(),
            input_schema,
        })
    }
}

/// Owned, executable-free extension metadata for a future runtime registry.
#[derive(Debug, Clone, PartialEq)]
pub struct OwnedExtensionDescriptor {
    id: OwnedExtensionId,
    provides: Vec<OwnedCapabilityId>,
    requires: Vec<OwnedCapabilityId>,
    tools: Vec<OwnedToolDescriptor>,
}

impl OwnedExtensionDescriptor {
    pub fn new(
        id: impl Into<String>,
        provides: Vec<String>,
        requires: Vec<String>,
        tools: Vec<OwnedToolDescriptor>,
    ) -> Result<Self, OwnedIdentifierError> {
        Ok(Self {
            id: OwnedExtensionId::try_new(id)?,
            provides: provides
                .into_iter()
                .map(OwnedCapabilityId::try_new)
                .collect::<Result<_, _>>()?,
            requires: requires
                .into_iter()
                .map(OwnedCapabilityId::try_new)
                .collect::<Result<_, _>>()?,
            tools,
        })
    }

    pub fn id(&self) -> &OwnedExtensionId {
        &self.id
    }

    pub fn provides(&self) -> &[OwnedCapabilityId] {
        &self.provides
    }

    pub fn requires(&self) -> &[OwnedCapabilityId] {
        &self.requires
    }

    pub fn tools(&self) -> &[OwnedToolDescriptor] {
        &self.tools
    }
}

/// Stable identifier for one HTTP route exposed by an extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RouteId(&'static str);

impl RouteId {
    pub const fn new(value: &'static str) -> Self {
        Self(value)
    }

    pub const fn as_str(self) -> &'static str {
        self.0
    }
}

impl fmt::Display for RouteId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

/// HTTP methods understood by the host-neutral extension route contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum HttpMethod {
    Get,
    Post,
    Put,
    Patch,
    Delete,
    Head,
    Options,
}

impl HttpMethod {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Patch => "PATCH",
            Self::Delete => "DELETE",
            Self::Head => "HEAD",
            Self::Options => "OPTIONS",
        }
    }
}

/// Static route registration contributed by an extension.
///
/// `path` uses literal segments and `{parameter}` segments, for example
/// `/api/items/{item_id}`. Matching is case-sensitive. Routes with the same
/// method may overlap only when one is strictly more specific; the host always
/// prefers the route with more literal path segments.
#[derive(Debug, Clone, Copy)]
pub struct ExtensionRouteDescriptor {
    pub id: RouteId,
    pub method: HttpMethod,
    pub path: &'static str,
    /// Mutating routes run under the host's serialized owner/workspace path.
    pub mutates: bool,
}

/// Generic request passed to an extension HTTP route handler.
#[derive(Debug, Clone, PartialEq)]
pub struct ExtensionHttpRequest {
    pub path_params: std::collections::BTreeMap<String, String>,
    /// Query keys can occur more than once; values retain their received order.
    pub query: std::collections::BTreeMap<String, Vec<String>>,
    /// Empty or absent request bodies are represented as JSON `null`.
    pub body: Value,
}

/// JSON response returned by an extension HTTP route handler.
#[derive(Debug, Clone, PartialEq)]
pub struct ExtensionHttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Value,
}

impl ExtensionHttpResponse {
    pub fn json(status: u16, body: Value) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body,
        }
    }
}

/// Successful route match, including parameters extracted from the path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionRouteMatch {
    route_id: RouteId,
    extension_id: ExtensionId,
    method: HttpMethod,
    mutates: bool,
    path_params: std::collections::BTreeMap<String, String>,
}

impl ExtensionRouteMatch {
    pub const fn route_id(&self) -> RouteId {
        self.route_id
    }

    pub const fn extension_id(&self) -> ExtensionId {
        self.extension_id
    }

    pub const fn method(&self) -> HttpMethod {
        self.method
    }

    pub const fn mutates(&self) -> bool {
        self.mutates
    }

    /// Host adapters serialize every extension route because a handler can
    /// mutate through its context even when route metadata says otherwise.
    pub const fn requires_serialization(&self) -> bool {
        true
    }

    pub fn path_params(&self) -> &std::collections::BTreeMap<String, String> {
        &self.path_params
    }
}

/// Static schema and description presented by a host transport for one tool.
#[derive(Debug, Clone, Copy)]
pub struct ToolDescriptor {
    pub id: ToolId,
    pub description: &'static str,
    /// JSON Schema encoded as JSON text. The registry requires a JSON object.
    pub input_schema: &'static str,
}

/// Static identity and dependency contract for an extension.
#[derive(Debug, Clone, Copy)]
pub struct ExtensionDescriptor {
    pub id: ExtensionId,
    pub provides: &'static [CapabilityId],
    pub requires: &'static [CapabilityId],
    pub tools: &'static [ToolDescriptor],
}

/// Generic record passed through a host context's scoped document store.
///
/// The host binds owner/workspace scope outside this value and must enforce
/// it on every read and write. Extensions cannot choose or widen that scope.
#[derive(Debug, Clone, PartialEq)]
pub struct ExtensionDocument {
    pub id: String,
    pub kind: String,
    pub value: Value,
}

/// Generic selectors accepted by a host's document search implementation.
///
/// `kind` is an exact match, `id_prefix` is a prefix match, and `text` asks
/// the host to search the document ID, kind, and serialized value. The host
/// may use an index, but it must only return documents visible in the scope
/// bound to the current operation. Results should be deterministic for a
/// fixed store state and query. `limit` is a maximum result count; zero returns
/// no results.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DocumentQuery {
    pub kind: Option<String>,
    pub id_prefix: Option<String>,
    pub text: Option<String>,
    pub limit: usize,
}

/// Host-attributed identity for the caller that initiated one extension call.
///
/// Values are supplied by the host from its trusted transport or adapter
/// context. Extensions must not derive or override them from tool arguments.
/// They describe attribution only; they are not an authorization decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvocationIdentity<'a> {
    /// Provider or adapter identity, such as `codex`, `claude`, or `unknown`.
    pub provider: &'a str,
    /// Optional trusted agent or principal identifier.
    pub principal: Option<&'a str>,
    /// Extension that immediately invoked this extension, when applicable.
    /// The registry supplies this value; tool arguments do not.
    pub caller_extension: Option<ExtensionId>,
}

impl InvocationIdentity<'_> {
    pub const UNKNOWN: Self = Self {
        provider: "unknown",
        principal: None,
        caller_extension: None,
    };
}

/// Opaque namespace token created only by the registry for one extension.
pub struct ExtensionScope {
    extension_id: ExtensionId,
}

impl ExtensionScope {
    /// Extension whose documents are visible through this scope.
    pub const fn extension_id(&self) -> ExtensionId {
        self.extension_id
    }
}

/// Host-provided document access bound to one owner/workspace operation.
///
/// Atomic batches prevent partial writes, but do not provide compare-and-swap
/// or protect a read-modify-write sequence from concurrent invocations. Hosts
/// must serialize mutating tool calls for the same owner/workspace across the
/// full invocation, including nested capability calls. This execution policy
/// prevents races in read-modify-write operations such as sequence allocation,
/// revision checks, and writing a projection with its event. Membrick currently
/// provides this through its staged writer and service locks.
pub trait ScopedDocumentStore {
    fn get(&self, id: &str) -> Result<Option<ExtensionDocument>, ExtensionError>;
    fn list(&self, kind: Option<&str>) -> Result<Vec<ExtensionDocument>, ExtensionError>;
    /// Search the documents visible in this operation's bound scope.
    ///
    /// The default keeps existing adapters source-compatible while failing
    /// explicitly if they have not implemented search. Hosts should override
    /// it before exposing search to extensions.
    fn search(&self, _query: &DocumentQuery) -> Result<Vec<ExtensionDocument>, ExtensionError> {
        Err(ExtensionError::unsupported(
            "scoped document search is not implemented by this host",
        ))
    }
    /// Atomically upsert all documents in one bound owner/workspace scope.
    ///
    /// Success means every document was committed. An error means none were
    /// committed. Duplicate document IDs in one batch must be rejected. A
    /// host that cannot guarantee all-or-none behavior must return an
    /// `Unsupported` error instead of performing a partial write. Atomicity
    /// does not provide compare-and-swap semantics; see the host serialization
    /// requirement on this trait.
    fn put_batch(&mut self, documents: Vec<ExtensionDocument>) -> Result<(), ExtensionError>;
}

/// Per-operation services supplied by the host.
///
/// `invoke` resolves an operation on a provided capability. Hosts should only
/// make declared, registered capabilities available and must keep all storage
/// scoped to this context's owner/workspace. For mutating tool calls, the host
/// must serialize the full invocation per owner/workspace, including nested
/// capability calls. This protects read-modify-write flows such as sequence
/// allocation, revision checks, and projection-plus-event writes. The API
/// provides no compare-and-swap operation; locking is the host execution
/// policy.
pub trait ExtensionContext {
    fn owner(&self) -> &str;
    fn workspace(&self) -> &str;
    /// Trusted optional session metadata for this invocation.
    ///
    /// Hosts must derive this from transport/session context rather than
    /// adding it to, or reading it from, extension tool arguments. Extensions
    /// may use it for session-scoped defaults while receiving the caller's
    /// JSON unchanged. The default preserves source compatibility.
    fn session_id(&self) -> Option<&str> {
        None
    }
    /// Trusted host attribution for the current invocation.
    ///
    /// The default preserves source compatibility for hosts that do not yet
    /// expose caller identity.
    fn invocation_identity(&self) -> InvocationIdentity<'_> {
        InvocationIdentity::UNKNOWN
    }
    fn documents(&mut self) -> &mut dyn ScopedDocumentStore;
    /// Host hook used by the registry to select an extension-specific document
    /// namespace. Extensions cannot construct this scope token.
    fn scoped_documents(&mut self, scope: &ExtensionScope) -> &mut dyn ScopedDocumentStore;
    fn invoke(
        &mut self,
        capability: CapabilityId,
        operation: &str,
        input: Value,
    ) -> Result<Value, ExtensionError>;
}

/// One extension's implementation for all tools in its descriptor.
pub trait ExtensionHandler: Sync {
    /// Initialize this statically registered extension before the host begins
    /// serving requests. The host calls hooks in dependency order and should
    /// call [`Self::shutdown`] in reverse order during orderly shutdown.
    ///
    /// Implementations that need per-host mutable state should use
    /// synchronization internally. Per-instance handler factories are outside
    /// the compiled-in v1 contract. If initialization returns an error, this
    /// handler must clean up any resources it acquired before returning; the
    /// registry only shuts down handlers whose initialization succeeded.
    fn initialize(&self) -> Result<(), ExtensionError> {
        Ok(())
    }

    /// Release resources acquired by [`Self::initialize`].
    fn shutdown(&self) -> Result<(), ExtensionError> {
        Ok(())
    }

    /// Optional custom tool definitions. Every declared tool is still listed:
    /// the registry fills omitted definitions from the static descriptor and
    /// rejects duplicate or undeclared names.
    fn tool_definitions(&self) -> Vec<ToolDefinition> {
        Vec::new()
    }

    /// Whether a tool changes extension state and therefore needs the host's
    /// serialized mutation path. The host holds the owner/workspace scope for
    /// the full invocation, including nested capability calls, so reads and
    /// writes within it cannot race with another mutation in that scope.
    fn tool_is_mutating(&self, _tool: ToolId) -> bool {
        false
    }

    /// Project a successful tool result into an optional opaque host
    /// notification. The host forwards the returned JSON without interpreting
    /// extension-specific fields.
    fn change_notification(&self, _tool: ToolId, _output: &Value) -> Option<Value> {
        None
    }

    /// HTTP routes contributed by this extension. The host owns the HTTP
    /// server, authentication, workspace selection, and serialization policy.
    fn http_routes(&self) -> &'static [ExtensionRouteDescriptor] {
        &[]
    }

    /// Handle one route declared by [`Self::http_routes`].
    fn handle_http(
        &self,
        _context: &mut dyn ExtensionContext,
        route: RouteId,
        _request: ExtensionHttpRequest,
    ) -> Result<ExtensionHttpResponse, ExtensionError> {
        Err(ExtensionError::unsupported(format!(
            "extension does not implement HTTP route `{route}`"
        )))
    }

    /// Kinds of records this extension adopts from a host's storage written
    /// before the extension owned them. A host matches a stored record's
    /// `filters.doc_kind` against these values. Each kind may be claimed by
    /// only one extension.
    fn legacy_record_kinds(&self) -> &'static [&'static str] {
        &[]
    }

    /// Convert one legacy host record into a document in this extension's
    /// scope. `record` is the host's stored record serialized as JSON with
    /// `doc_id`, `content` and `filters`. Return `Ok(None)` for a record
    /// outside the `owner`/`workspace` scope, which the host then leaves in
    /// place. The host writes returned documents through the extension's
    /// scoped store, so the extension cannot widen its scope here.
    fn adopt_legacy_record(
        &self,
        _owner: &str,
        _workspace: &str,
        _record: &Value,
    ) -> Result<Option<ExtensionDocument>, ExtensionError> {
        Ok(None)
    }

    fn call(
        &self,
        context: &mut dyn ExtensionContext,
        tool: ToolId,
        input: Value,
    ) -> Result<Value, ExtensionError>;

    /// Handle an operation requested through one of this extension's
    /// capabilities. Extensions that expose no cross-extension operations can
    /// keep the default implementation.
    fn invoke(
        &self,
        _context: &mut dyn ExtensionContext,
        _capability: CapabilityId,
        _operation: &str,
        _input: Value,
    ) -> Result<Value, ExtensionError> {
        Err(ExtensionError::new(
            "extension does not implement the requested capability operation",
        ))
    }
}

/// Link-time registration of a descriptor and its stateless handler.
#[derive(Clone, Copy)]
pub struct ExtensionRegistration {
    pub descriptor: &'static ExtensionDescriptor,
    pub handler: &'static dyn ExtensionHandler,
}

/// Extensions contribute entries from any linked crate with `linkme`:
/// `#[distributed_slice(membrick_extension_api::EXTENSIONS)]`.
#[distributed_slice]
pub static EXTENSIONS: [ExtensionRegistration];

/// A validated set of linked extensions and generic tool/HTTP routes.
/// Validated extension set for one host instance.
///
/// The host must call [`Self::initialize`] successfully before dispatching
/// extension tools, and stop dispatching before calling [`Self::shutdown`].
/// The registry records lifecycle state but does not gate dispatch, preserving
/// compatibility for existing hosts.
pub struct ExtensionRegistry {
    ordered: Vec<&'static ExtensionRegistration>,
    tools: HashMap<ToolId, &'static ExtensionRegistration>,
    capabilities: HashMap<CapabilityId, &'static ExtensionRegistration>,
    approved_capabilities: HashMap<ExtensionId, BTreeSet<CapabilityId>>,
    tool_definitions: Vec<ToolDefinition>,
    routes: Vec<RegisteredRoute>,
    routes_by_id: HashMap<RouteId, usize>,
    lifecycle: Mutex<LifecycleState>,
}

struct RegistryInvocationContext<'a> {
    inner: &'a mut dyn ExtensionContext,
    registry: &'a ExtensionRegistry,
    executing_extension: ExtensionId,
    caller_extension: Option<ExtensionId>,
    scope: ExtensionScope,
}

impl ExtensionContext for RegistryInvocationContext<'_> {
    fn owner(&self) -> &str {
        self.inner.owner()
    }
    fn workspace(&self) -> &str {
        self.inner.workspace()
    }
    fn session_id(&self) -> Option<&str> {
        self.inner.session_id()
    }
    fn invocation_identity(&self) -> InvocationIdentity<'_> {
        let mut identity = self.inner.invocation_identity();
        identity.caller_extension = self.caller_extension;
        identity
    }
    fn documents(&mut self) -> &mut dyn ScopedDocumentStore {
        self.inner.scoped_documents(&self.scope)
    }
    fn scoped_documents(&mut self, scope: &ExtensionScope) -> &mut dyn ScopedDocumentStore {
        self.inner.scoped_documents(scope)
    }
    fn invoke(
        &mut self,
        capability: CapabilityId,
        operation: &str,
        input: Value,
    ) -> Result<Value, ExtensionError> {
        let registry = self.registry;
        let caller = self.executing_extension;
        registry.invoke_from(self, caller, capability, operation, input)
    }
}

struct RegisteredRoute {
    descriptor: &'static ExtensionRouteDescriptor,
    registration: &'static ExtensionRegistration,
    segments: Vec<RouteSegment>,
}

#[derive(Clone, Copy)]
enum RouteSegment {
    Literal(&'static str),
    Parameter(&'static str),
}

fn parse_route_pattern(path: &'static str) -> Result<Vec<RouteSegment>, String> {
    if path == "/" {
        return Ok(Vec::new());
    }
    if !path.starts_with('/') || path.ends_with('/') || path.contains(['?', '#']) {
        return Err(format!("invalid route path pattern `{path}`"));
    }
    let mut parameter_names = BTreeSet::new();
    path[1..]
        .split('/')
        .map(|segment| {
            if segment.is_empty() || segment == "." || segment == ".." {
                return Err(format!("invalid route path pattern `{path}`"));
            }
            if let Some(parameter) = segment
                .strip_prefix('{')
                .and_then(|value| value.strip_suffix('}'))
            {
                let mut chars = parameter.chars();
                let valid_first = chars
                    .next()
                    .is_some_and(|character| character == '_' || character.is_ascii_alphabetic());
                if !valid_first
                    || !chars.all(|character| character == '_' || character.is_ascii_alphanumeric())
                {
                    return Err(format!("invalid route parameter in path pattern `{path}`"));
                }
                if !parameter_names.insert(parameter) {
                    return Err(format!(
                        "duplicate route parameter in path pattern `{path}`"
                    ));
                }
                Ok(RouteSegment::Parameter(parameter))
            } else if segment.contains(['{', '}']) {
                Err(format!("invalid route parameter in path pattern `{path}`"))
            } else {
                Ok(RouteSegment::Literal(segment))
            }
        })
        .collect()
}

fn route_patterns_conflict(left: &[RouteSegment], right: &[RouteSegment]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .filter(|segment| matches!(segment, RouteSegment::Literal(_)))
            .count()
            == right
                .iter()
                .filter(|segment| matches!(segment, RouteSegment::Literal(_)))
                .count()
        && left
            .iter()
            .zip(right)
            .all(|(left, right)| match (left, right) {
                (RouteSegment::Literal(left), RouteSegment::Literal(right)) => left == right,
                _ => true,
            })
}

fn split_route_path(path: &str) -> Option<Vec<&str>> {
    if path == "/" {
        return Some(Vec::new());
    }
    if !path.starts_with('/') || path.ends_with('/') || path.contains(['?', '#']) {
        return None;
    }
    let segments = path[1..].split('/').collect::<Vec<_>>();
    (!segments.iter().any(|segment| segment.is_empty())).then_some(segments)
}

fn decode_route_parameter(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let high = *bytes.get(index + 1)?;
            let low = *bytes.get(index + 2)?;
            let hex = |byte: u8| match byte {
                b'0'..=b'9' => Some(byte - b'0'),
                b'a'..=b'f' => Some(byte - b'a' + 10),
                b'A'..=b'F' => Some(byte - b'A' + 10),
                _ => None,
            };
            decoded.push((hex(high)? << 4) | hex(low)?);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

enum LifecycleState {
    NotStarted,
    Starting,
    Started,
    Failed(ExtensionError),
    Stopping,
    Stopped(Result<(), ExtensionError>),
}

impl ExtensionRegistry {
    /// Collect and validate registrations contributed by linked crates.
    pub fn from_linked() -> Result<Self, RegistryError> {
        Self::new(EXTENSIONS.iter().collect())
    }

    /// Validate a set of registrations and sort it in dependency order.
    ///
    /// This constructor also allows a host to supply an explicit set, for
    /// example in an embedded application that does not use link-time
    /// collection.
    pub fn new(registrations: Vec<&'static ExtensionRegistration>) -> Result<Self, RegistryError> {
        // Linked native registrations are part of the trusted executable
        // composition. Their declared dependency edges are therefore the
        // host-approved grants for this static-only path. This does not load
        // or sandbox extensions. Runtime admission uses the explicit policy
        // constructor below.
        let approved_capabilities = registrations
            .iter()
            .map(|registration| {
                (
                    registration.descriptor.id,
                    registration.descriptor.requires.iter().copied().collect(),
                )
            })
            .collect();
        Self::new_with_approved_capabilities(registrations, approved_capabilities)
    }

    /// Validate registrations using a host-owned capability grant set.
    /// A nested capability call must be both declared by its caller and
    /// present in this approval map.
    pub fn new_with_approved_capabilities(
        registrations: Vec<&'static ExtensionRegistration>,
        approved_capabilities: HashMap<ExtensionId, BTreeSet<CapabilityId>>,
    ) -> Result<Self, RegistryError> {
        let mut extension_ids = BTreeSet::new();
        let mut capabilities = HashMap::new();
        let mut tools = HashMap::new();
        let mut route_ids = BTreeSet::new();
        let mut routes = Vec::<RegisteredRoute>::new();
        let mut definitions_by_extension = HashMap::<ExtensionId, Vec<ToolDefinition>>::new();

        for registration in &registrations {
            let descriptor = registration.descriptor;
            if descriptor.id.as_str().trim().is_empty() {
                return Err(RegistryError::new("extension ID must not be empty"));
            }
            if !extension_ids.insert(descriptor.id) {
                return Err(RegistryError::new(format!(
                    "extension ID `{}` is registered more than once",
                    descriptor.id.as_str()
                )));
            }
            if descriptor.provides.is_empty() {
                return Err(RegistryError::new(format!(
                    "extension `{}` does not provide a capability",
                    descriptor.id.as_str()
                )));
            }

            let mut own_capabilities = BTreeSet::new();
            for capability in descriptor.provides {
                if capability.as_str().trim().is_empty() {
                    return Err(RegistryError::new(format!(
                        "extension `{}` declares an empty capability ID",
                        descriptor.id
                    )));
                }
                if !own_capabilities.insert(*capability) {
                    return Err(RegistryError::new(format!(
                        "extension `{}` declares capability `{}` more than once",
                        descriptor.id.as_str(),
                        capability.as_str()
                    )));
                }
                if capabilities.insert(*capability, *registration).is_some() {
                    return Err(RegistryError::new(format!(
                        "capability `{}` has more than one provider",
                        capability.as_str()
                    )));
                }
            }

            let mut own_tools = BTreeSet::new();
            for tool in descriptor.tools {
                if tool.id.as_str().trim().is_empty() {
                    return Err(RegistryError::new(format!(
                        "extension `{}` declares an empty tool ID",
                        descriptor.id.as_str()
                    )));
                }
                if !own_tools.insert(tool.id) {
                    return Err(RegistryError::new(format!(
                        "extension `{}` declares tool `{}` more than once",
                        descriptor.id.as_str(),
                        tool.id.as_str()
                    )));
                }
                let schema = serde_json::from_str::<Value>(tool.input_schema).map_err(|_| {
                    RegistryError::new(format!(
                        "tool `{}` in extension `{}` has invalid JSON Schema JSON",
                        tool.id.as_str(),
                        descriptor.id
                    ))
                })?;
                if !schema.is_object() {
                    return Err(RegistryError::new(format!(
                        "tool `{}` in extension `{}` has a JSON Schema that is not an object",
                        tool.id.as_str(),
                        descriptor.id
                    )));
                }
                if tools.insert(tool.id, *registration).is_some() {
                    return Err(RegistryError::new(format!(
                        "tool `{}` is registered more than once",
                        tool.id.as_str()
                    )));
                }
            }

            for route in registration.handler.http_routes() {
                if route.id.as_str().trim().is_empty() {
                    return Err(RegistryError::new(format!(
                        "extension `{}` declares an empty HTTP route ID",
                        descriptor.id
                    )));
                }
                if !route_ids.insert(route.id) {
                    return Err(RegistryError::new(format!(
                        "HTTP route ID `{}` is registered more than once",
                        route.id
                    )));
                }
                let segments = parse_route_pattern(route.path).map_err(RegistryError::new)?;
                if let Some(existing) = routes.iter().find(|existing| {
                    existing.descriptor.method == route.method
                        && route_patterns_conflict(&existing.segments, &segments)
                }) {
                    return Err(RegistryError::new(format!(
                        "HTTP routes `{}` (`{} {}`) and `{}` (`{} {}`) overlap",
                        existing.descriptor.id,
                        existing.descriptor.method.as_str(),
                        existing.descriptor.path,
                        route.id,
                        route.method.as_str(),
                        route.path
                    )));
                }
                routes.push(RegisteredRoute {
                    descriptor: route,
                    registration,
                    segments,
                });
            }

            let declared_tool_ids = descriptor
                .tools
                .iter()
                .map(|tool| tool.id.as_str())
                .collect::<BTreeSet<_>>();
            let mut custom_definitions = HashMap::new();
            for definition in registration.handler.tool_definitions().into_iter() {
                if !definition.input_schema.is_object() {
                    return Err(RegistryError::new(format!(
                        "extension `{}` defines tool `{}` with a JSON Schema that is not an object",
                        descriptor.id, definition.name
                    )));
                }
                if !declared_tool_ids.contains(definition.name.as_str()) {
                    return Err(RegistryError::new(format!(
                        "extension `{}` exposes tool `{}` absent from its static descriptor",
                        descriptor.id, definition.name
                    )));
                }
                let name = definition.name.clone();
                if custom_definitions
                    .insert(name.clone(), definition)
                    .is_some()
                {
                    return Err(RegistryError::new(format!(
                        "extension `{}` defines tool `{name}` more than once",
                        descriptor.id
                    )));
                }
            }

            let definitions = descriptor
                .tools
                .iter()
                .map(|tool| {
                    custom_definitions
                        .remove(tool.id.as_str())
                        .unwrap_or_else(|| ToolDefinition {
                            name: tool.id.as_str().to_owned(),
                            description: tool.description.to_owned(),
                            input_schema: serde_json::from_str(tool.input_schema)
                                .expect("tool schemas are validated above"),
                        })
                })
                .collect();
            definitions_by_extension.insert(descriptor.id, definitions);
        }

        let mut dependencies = vec![BTreeSet::<usize>::new(); registrations.len()];
        let indexes_by_id: HashMap<_, _> = registrations
            .iter()
            .enumerate()
            .map(|(index, registration)| (registration.descriptor.id, index))
            .collect();
        let mut dependents = vec![Vec::<usize>::new(); registrations.len()];

        for (index, registration) in registrations.iter().enumerate() {
            let descriptor = registration.descriptor;
            let mut seen_requirements = BTreeSet::new();
            for required in descriptor.requires {
                if !seen_requirements.insert(*required) {
                    return Err(RegistryError::new(format!(
                        "extension `{}` repeats required capability `{}`",
                        descriptor.id,
                        required.as_str()
                    )));
                }
                let provider = capabilities.get(required).ok_or_else(|| {
                    RegistryError::new(format!(
                        "extension `{}` requires missing capability `{}`",
                        descriptor.id,
                        required.as_str()
                    ))
                })?;
                let provider_index = indexes_by_id[&provider.descriptor.id];
                if provider_index != index {
                    dependencies[index].insert(provider_index);
                    dependents[provider_index].push(index);
                } else {
                    return Err(RegistryError::new(format!(
                        "extension `{}` requires its own capability `{}`",
                        descriptor.id,
                        required.as_str()
                    )));
                }
            }
        }

        // Kahn topological sort with registration order as the stable tie
        // breaker. A remaining vertex indicates a dependency cycle.
        let mut ready = (0..registrations.len())
            .filter(|index| dependencies[*index].is_empty())
            .collect::<Vec<_>>();
        let mut sorted = Vec::with_capacity(registrations.len());
        while !ready.is_empty() {
            let index = ready.remove(0);
            sorted.push(registrations[index]);
            for dependent in &dependents[index] {
                dependencies[*dependent].remove(&index);
                if dependencies[*dependent].is_empty()
                    && !sorted
                        .iter()
                        .any(|entry| entry.descriptor.id == registrations[*dependent].descriptor.id)
                    && !ready.contains(dependent)
                {
                    ready.push(*dependent);
                }
            }
            ready.sort_by_key(|candidate| *candidate);
        }
        if sorted.len() != registrations.len() {
            let cycle = registrations
                .iter()
                .enumerate()
                .filter(|(index, _)| !dependencies[*index].is_empty())
                .map(|(_, registration)| registration.descriptor.id.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(RegistryError::new(format!(
                "extension dependency cycle detected among: {cycle}"
            )));
        }

        let tool_definitions = sorted
            .iter()
            .flat_map(|registration| {
                definitions_by_extension
                    .remove(&registration.descriptor.id)
                    .unwrap_or_default()
            })
            .collect();

        routes.sort_by_key(|route| {
            std::cmp::Reverse(
                route
                    .segments
                    .iter()
                    .filter(|segment| matches!(segment, RouteSegment::Literal(_)))
                    .count(),
            )
        });
        let routes_by_id = routes
            .iter()
            .enumerate()
            .map(|(index, route)| (route.descriptor.id, index))
            .collect();

        Ok(Self {
            ordered: sorted,
            tools,
            capabilities,
            approved_capabilities,
            tool_definitions,
            routes,
            routes_by_id,
            lifecycle: Mutex::new(LifecycleState::NotStarted),
        })
    }

    /// Extensions in deterministic dependency order.
    pub fn extensions(&self) -> impl Iterator<Item = &'static ExtensionRegistration> + '_ {
        self.ordered.iter().copied()
    }

    /// The extension that adopts each legacy record kind, in dependency
    /// order. Fails when two extensions claim the same kind.
    pub fn legacy_record_adopters(
        &self,
    ) -> Result<BTreeMap<&'static str, &'static ExtensionRegistration>, ExtensionError> {
        let mut adopters = BTreeMap::new();
        for registration in self.extensions() {
            for &kind in registration.handler.legacy_record_kinds() {
                if let Some(previous) = adopters.insert(kind, registration) {
                    return Err(ExtensionError::new(format!(
                        "legacy record kind `{kind}` is claimed by both `{}` and `{}`",
                        previous.descriptor.id, registration.descriptor.id
                    )));
                }
            }
        }
        Ok(adopters)
    }

    /// Capabilities declared by registered extensions, in deterministic
    /// extension and descriptor order.
    pub fn capabilities(&self) -> impl Iterator<Item = CapabilityId> + '_ {
        self.ordered
            .iter()
            .flat_map(|registration| registration.descriptor.provides.iter().copied())
    }

    /// Initialize every extension exactly once for this registry instance, in
    /// dependency order.
    ///
    /// Repeated calls after success return `Ok(())` without rerunning hooks.
    /// If an initializer fails, already initialized extensions are shut down
    /// once in reverse order; the failure is recorded and returned again on
    /// later calls. The failing initializer must clean up its own partial
    /// resources. An initialization failure is terminal for this registry.
    pub fn initialize(&self) -> Result<(), ExtensionError> {
        let mut lifecycle = self
            .lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match &*lifecycle {
            LifecycleState::NotStarted => *lifecycle = LifecycleState::Starting,
            LifecycleState::Started => return Ok(()),
            LifecycleState::Failed(error) => return Err(error.clone()),
            LifecycleState::Stopped(_) => {
                return Err(ExtensionError::with_kind(
                    ExtensionErrorKind::Conflict,
                    "extension registry has already been shut down",
                ));
            }
            LifecycleState::Starting | LifecycleState::Stopping => {
                return Err(ExtensionError::with_kind(
                    ExtensionErrorKind::Conflict,
                    "extension registry lifecycle hook is already in progress",
                ));
            }
        }

        let mut initialized: Vec<&'static ExtensionRegistration> = Vec::new();
        for registration in &self.ordered {
            if let Err(error) = registration.handler.initialize() {
                for initialized_registration in initialized.into_iter().rev() {
                    let _ = initialized_registration.handler.shutdown();
                }
                *lifecycle = LifecycleState::Failed(error.clone());
                return Err(error);
            }
            initialized.push(*registration);
        }
        *lifecycle = LifecycleState::Started;
        Ok(())
    }

    /// Shut down initialized extensions exactly once for this registry
    /// instance, in reverse dependency order. All hooks are attempted; the first error is recorded and
    /// returned. Repeated calls return the recorded result without rerunning
    /// hooks. Calling this before initialization marks the registry stopped
    /// without invoking any extension hooks. After a failed initialization,
    /// this is a no-op because successful extensions were already rolled back.
    pub fn shutdown(&self) -> Result<(), ExtensionError> {
        let mut lifecycle = self
            .lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match &*lifecycle {
            LifecycleState::NotStarted => {
                *lifecycle = LifecycleState::Stopped(Ok(()));
                return Ok(());
            }
            LifecycleState::Failed(_) => return Ok(()),
            LifecycleState::Stopped(result) => return result.clone(),
            LifecycleState::Started => *lifecycle = LifecycleState::Stopping,
            LifecycleState::Starting | LifecycleState::Stopping => {
                return Err(ExtensionError::with_kind(
                    ExtensionErrorKind::Conflict,
                    "extension registry lifecycle hook is already in progress",
                ));
            }
        }

        let mut first_error = None;
        for registration in self.ordered.iter().rev() {
            if let Err(error) = registration.handler.shutdown() {
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
        let result = first_error.map_or(Ok(()), Err);
        *lifecycle = LifecycleState::Stopped(result.clone());
        result
    }

    /// Return the provider for a registered capability.
    pub fn capability_provider(&self, id: CapabilityId) -> Option<&'static ExtensionRegistration> {
        self.capabilities.get(&id).copied()
    }

    /// Whether a tool name is registered by any linked extension.
    pub fn has_tool(&self, id: ToolId) -> bool {
        self.tools.contains_key(&id)
    }

    /// Whether a tool name is registered. Use this for names received at
    /// runtime; [`ToolId::new`] is intended for static extension declarations.
    pub fn has_tool_name(&self, name: &str) -> bool {
        self.tools.keys().any(|tool| tool.as_str() == name)
    }

    /// Extension that declared a tool.
    pub fn tool_provider(&self, id: ToolId) -> Option<ExtensionId> {
        self.tools
            .get(&id)
            .map(|registration| registration.descriptor.id)
    }

    /// Extension that declared a runtime tool name.
    pub fn tool_provider_name(&self, name: &str) -> Option<ExtensionId> {
        self.tools
            .iter()
            .find(|(tool, _)| tool.as_str() == name)
            .map(|(_, registration)| registration.descriptor.id)
    }

    /// Whether a registered extension tool requires serialized execution.
    ///
    /// Extension metadata cannot opt out of serialization: all extension
    /// calls are treated as potentially mutating by the host.
    pub fn tool_is_mutating_name(&self, name: &str) -> Option<bool> {
        self.tools
            .keys()
            .find(|tool| tool.as_str() == name)
            .map(|_| true)
    }

    /// Ask the owning extension for an opaque notification after a successful
    /// tool call. Unknown tools and handlers without notifications return None.
    pub fn change_notification(&self, name: &str, output: &Value) -> Option<Value> {
        let (tool, registration) = self.tools.iter().find(|(tool, _)| tool.as_str() == name)?;
        registration.handler.change_notification(*tool, output)
    }

    /// Registered HTTP routes ordered by specificity, preserving registration
    /// order among routes with equal specificity.
    pub fn http_routes(
        &self,
    ) -> impl Iterator<Item = (ExtensionId, &'static ExtensionRouteDescriptor)> + '_ {
        self.routes
            .iter()
            .map(|route| (route.registration.descriptor.id, route.descriptor))
    }

    /// Match a method and path against registered extension routes.
    pub fn match_http_route(&self, method: HttpMethod, path: &str) -> Option<ExtensionRouteMatch> {
        let path_segments = split_route_path(path)?;
        self.routes.iter().find_map(|route| {
            if route.descriptor.method != method || route.segments.len() != path_segments.len() {
                return None;
            }
            let mut path_params = std::collections::BTreeMap::new();
            for (pattern, value) in route.segments.iter().zip(path_segments.iter()) {
                match pattern {
                    RouteSegment::Literal(expected) if expected == value => {}
                    RouteSegment::Parameter(name) => {
                        path_params.insert((*name).to_owned(), decode_route_parameter(value)?);
                    }
                    RouteSegment::Literal(_) => return None,
                }
            }
            Some(ExtensionRouteMatch {
                route_id: route.descriptor.id,
                extension_id: route.registration.descriptor.id,
                method,
                mutates: route.descriptor.mutates,
                path_params,
            })
        })
    }

    /// Dispatch a previously matched HTTP route to its owning extension.
    pub fn call_http_route(
        &self,
        context: &mut dyn ExtensionContext,
        route: &ExtensionRouteMatch,
        mut request: ExtensionHttpRequest,
    ) -> Result<ExtensionHttpResponse, ExtensionError> {
        let index = self.routes_by_id.get(&route.route_id).ok_or_else(|| {
            ExtensionError::new(format!("unknown extension HTTP route `{}`", route.route_id))
        })?;
        let registered = &self.routes[*index];
        if registered.registration.descriptor.id != route.extension_id
            || registered.descriptor.method != route.method
            || registered.descriptor.mutates != route.mutates
        {
            return Err(ExtensionError::with_kind(
                ExtensionErrorKind::InvalidInput,
                "HTTP route match does not belong to this registry",
            ));
        }
        request.path_params = route.path_params.clone();
        let extension_id = registered.registration.descriptor.id;
        tracing::info!(
            event = "extension_invocation",
            extension_id = extension_id.as_str(),
            caller_extension = "host",
            invocation_kind = "http_route",
            operation = registered.descriptor.id.as_str(),
            decision = "allow",
            result = "started"
        );
        let mut scoped = RegistryInvocationContext {
            inner: context,
            registry: self,
            executing_extension: extension_id,
            caller_extension: None,
            scope: ExtensionScope { extension_id },
        };
        let result = registered.registration.handler.handle_http(
            &mut scoped,
            registered.descriptor.id,
            request,
        );
        log_extension_result(
            extension_id,
            None,
            "http_route",
            LoggedOperation::Static(registered.descriptor.id.as_str()),
            &result,
        );
        result
    }

    /// Complete tool definitions for registered extensions.
    ///
    /// Definitions are returned in dependency order and preserve each
    /// extension's descriptor order. Handler overrides are captured and
    /// validated when the registry is constructed; any omitted definitions
    /// are generated from the static descriptors.
    pub fn tool_definitions(&self) -> Vec<ToolDefinition> {
        self.tool_definitions.clone()
    }

    /// Tool descriptors in deterministic extension and declaration order.
    pub fn tools(&self) -> impl Iterator<Item = &'static ToolDescriptor> + '_ {
        self.ordered
            .iter()
            .flat_map(|registration| registration.descriptor.tools.iter())
    }

    /// Dispatch a generic tool request to the extension that declared it.
    pub fn call(
        &self,
        context: &mut dyn ExtensionContext,
        tool: ToolId,
        input: Value,
    ) -> Result<Value, ExtensionError> {
        let registration = self.tools.get(&tool).ok_or_else(|| {
            ExtensionError::new(format!("unknown extension tool `{}`", tool.as_str()))
        })?;
        self.call_as(context, registration, tool, input)
    }

    /// Dispatch a tool name received from a transport.
    pub fn call_named(
        &self,
        context: &mut dyn ExtensionContext,
        name: &str,
        input: Value,
    ) -> Result<Value, ExtensionError> {
        let (tool, registration) = self
            .tools
            .iter()
            .find(|(tool, _)| tool.as_str() == name)
            .ok_or_else(|| ExtensionError::new(format!("unknown extension tool `{name}`")))?;
        self.call_as(context, registration, *tool, input)
    }

    fn call_as(
        &self,
        context: &mut dyn ExtensionContext,
        registration: &'static ExtensionRegistration,
        tool: ToolId,
        input: Value,
    ) -> Result<Value, ExtensionError> {
        let extension_id = registration.descriptor.id;
        tracing::info!(
            event = "extension_invocation",
            extension_id = extension_id.as_str(),
            caller_extension = "host",
            invocation_kind = "tool",
            operation = tool.as_str(),
            decision = "allow",
            result = "started"
        );
        let mut scoped = RegistryInvocationContext {
            inner: context,
            registry: self,
            executing_extension: extension_id,
            caller_extension: None,
            scope: ExtensionScope { extension_id },
        };
        let result = registration.handler.call(&mut scoped, tool, input);
        log_extension_result(
            extension_id,
            None,
            "tool",
            LoggedOperation::Static(tool.as_str()),
            &result,
        );
        result
    }

    fn invoke_from(
        &self,
        context: &mut dyn ExtensionContext,
        caller: ExtensionId,
        capability: CapabilityId,
        operation: &str,
        input: Value,
    ) -> Result<Value, ExtensionError> {
        let caller_registration = self
            .ordered
            .iter()
            .find(|registration| registration.descriptor.id == caller)
            .copied()
            .ok_or_else(|| ExtensionError::new("calling extension is not registered"))?;
        let declared = caller_registration
            .descriptor
            .requires
            .contains(&capability);
        let approved = self
            .approved_capabilities
            .get(&caller)
            .is_some_and(|grants| grants.contains(&capability));
        if !declared || !approved {
            tracing::warn!(
                event = "extension_capability_invocation",
                caller_extension = caller.as_str(),
                capability = capability.as_str(),
                operation = capability_operation_label(operation),
                decision = "deny",
                result = "denied",
                reason = if !declared {
                    "undeclared"
                } else {
                    "not_approved"
                }
            );
            return Err(ExtensionError::with_kind(
                ExtensionErrorKind::Forbidden,
                format!("extension `{caller}` is not authorized for capability `{capability}`"),
            ));
        }
        let provider = self.capabilities.get(&capability).copied().ok_or_else(|| {
            ExtensionError::new(format!("unknown extension capability `{capability}`"))
        })?;
        let provider_id = provider.descriptor.id;
        tracing::info!(
            event = "extension_capability_invocation",
            caller_extension = caller.as_str(),
            target_extension = provider_id.as_str(),
            capability = capability.as_str(),
            operation = capability_operation_label(operation),
            decision = "allow",
            result = "started"
        );
        let mut scoped = RegistryInvocationContext {
            inner: context,
            registry: self,
            executing_extension: provider_id,
            caller_extension: Some(caller),
            scope: ExtensionScope {
                extension_id: provider_id,
            },
        };
        let result = provider
            .handler
            .invoke(&mut scoped, capability, operation, input);
        log_extension_result(
            provider_id,
            Some(caller),
            "capability",
            LoggedOperation::Capability(capability),
            &result,
        );
        result
    }

    /// Invoke an operation through the provider of a named capability.
    ///
    /// This context-free entry point is intentionally denied: only a
    /// registry-created invocation frame can establish an extension caller.
    /// Extensions should call through [`ExtensionContext::invoke`]. Trusted
    /// host code that needs to start a capability invocation can use
    /// [`Self::invoke_from_host`].
    pub fn invoke(
        &self,
        _context: &mut dyn ExtensionContext,
        _capability: CapabilityId,
        _operation: &str,
        _input: Value,
    ) -> Result<Value, ExtensionError> {
        Err(ExtensionError::with_kind(
            ExtensionErrorKind::Forbidden,
            "capability invocation requires a registry-created caller frame",
        ))
    }

    /// Start a capability invocation from trusted host code.
    ///
    /// This is an explicit host boundary. Extension code receives a
    /// registry-created context and can only invoke capabilities through that
    /// context, which records the immediate extension caller and checks its
    /// declared and approved grants.
    pub fn invoke_from_host(
        &self,
        context: &mut dyn ExtensionContext,
        capability: CapabilityId,
        operation: &str,
        input: Value,
    ) -> Result<Value, ExtensionError> {
        let provider = self.capabilities.get(&capability).copied().ok_or_else(|| {
            ExtensionError::new(format!("unknown extension capability `{capability}`"))
        })?;
        let extension_id = provider.descriptor.id;
        tracing::info!(
            event = "extension_capability_invocation",
            caller_extension = "host",
            target_extension = extension_id.as_str(),
            capability = capability.as_str(),
            operation = capability_operation_label(operation),
            decision = "allow",
            result = "started"
        );
        let mut scoped = RegistryInvocationContext {
            inner: context,
            registry: self,
            executing_extension: extension_id,
            caller_extension: None,
            scope: ExtensionScope { extension_id },
        };
        let result = provider
            .handler
            .invoke(&mut scoped, capability, operation, input);
        log_extension_result(
            extension_id,
            None,
            "host_capability",
            LoggedOperation::Capability(capability),
            &result,
        );
        result
    }
}

#[derive(Clone, Copy)]
enum LoggedOperation<'a> {
    /// Stable operation identifiers declared in the linked extension.
    Static(&'a str),
    /// Caller-supplied capability operation names are deliberately omitted.
    Capability(CapabilityId),
}

fn logged_operation_fields(operation: LoggedOperation<'_>) -> (&str, &str) {
    match operation {
        LoggedOperation::Static(operation) => (operation, ""),
        LoggedOperation::Capability(capability) => ("capability", capability.as_str()),
    }
}

fn capability_operation_label(_caller_operation: &str) -> &'static str {
    "capability"
}

fn log_extension_result<T>(
    extension: ExtensionId,
    caller: Option<ExtensionId>,
    invocation_kind: &'static str,
    operation: LoggedOperation<'_>,
    result: &Result<T, ExtensionError>,
) {
    let (operation_name, capability_name) = logged_operation_fields(operation);
    tracing::info!(
        event = "extension_invocation",
        extension_id = extension.as_str(),
        caller_extension = caller.map(ExtensionId::as_str).unwrap_or("host"),
        invocation_kind,
        operation = operation_name,
        capability = capability_name,
        result = if result.is_ok() { "ok" } else { "error" },
        error_kind = result
            .as_ref()
            .err()
            .map(|error| format!("{:?}", error.kind())),
    );
}

/// Error returned by an extension handler or host context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionError {
    kind: ExtensionErrorKind,
    message: String,
}

/// Stable, domain-neutral classification for extension and host failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtensionErrorKind {
    InvalidInput,
    NotFound,
    Conflict,
    Forbidden,
    Unavailable,
    Unsupported,
    Internal,
}

impl ExtensionError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            kind: ExtensionErrorKind::Internal,
            message: message.into(),
        }
    }

    pub fn with_kind(kind: ExtensionErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::with_kind(ExtensionErrorKind::Unsupported, message)
    }

    pub const fn kind(&self) -> ExtensionErrorKind {
        self.kind
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for ExtensionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for ExtensionError {}

/// Error found while building the generic extension registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryError {
    message: String,
}

impl RegistryError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for RegistryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for RegistryError {}

/// Metadata-only registry snapshot for owned extension descriptors.
///
/// This catalog validates and indexes declarations but stores no extension
/// handlers and exposes no invocation method. Building or replacing a catalog
/// never loads or dispatches downloaded code. Hosts must publish a successfully
/// built replacement as one snapshot; a rejected replacement leaves the prior
/// snapshot unchanged.
#[derive(Debug, Clone)]
pub struct OwnedExtensionRegistry {
    generation: u64,
    static_extensions: Vec<OwnedExtensionDescriptor>,
    dynamic_extensions: Vec<OwnedExtensionDescriptor>,
    ordered_extensions: Vec<OwnedExtensionDescriptor>,
    tool_owners: HashMap<OwnedToolId, OwnedExtensionId>,
}

const MAX_OWNED_EXTENSIONS: usize = 256;
const MAX_OWNED_TOOLS_PER_EXTENSION: usize = 128;
const MAX_OWNED_CAPABILITIES_PER_EXTENSION: usize = 128;
const MAX_OWNED_TOOL_DESCRIPTION_BYTES: usize = 4096;
const MAX_OWNED_TOOL_SCHEMA_BYTES: usize = 64 * 1024;

impl OwnedExtensionRegistry {
    /// Build a metadata-only registry with no statically linked entries.
    pub fn new(dynamic_extensions: Vec<OwnedExtensionDescriptor>) -> Result<Self, RegistryError> {
        Self::build(Vec::new(), dynamic_extensions, 0)
    }

    /// Build a metadata-only view that checks owned descriptors against the
    /// existing statically linked registry without changing its dispatch path.
    pub fn with_static_registry(
        static_registry: &ExtensionRegistry,
        dynamic_extensions: Vec<OwnedExtensionDescriptor>,
    ) -> Result<Self, RegistryError> {
        let static_extensions = static_registry
            .extensions()
            .map(|registration| owned_descriptor_from_static(registration.descriptor))
            .collect::<Result<Vec<_>, _>>()?;
        Self::build(static_extensions, dynamic_extensions, 0)
    }

    /// Validate a complete replacement set before changing this snapshot.
    /// On any validation error, the generation and existing metadata remain
    /// unchanged. This only replaces metadata; it does not activate handlers.
    pub fn replace_dynamic(
        &mut self,
        dynamic_extensions: Vec<OwnedExtensionDescriptor>,
    ) -> Result<(), RegistryError> {
        let generation = self
            .generation
            .checked_add(1)
            .ok_or_else(|| RegistryError::new("owned registry generation exhausted"))?;
        let replacement = Self::build(
            self.static_extensions.clone(),
            dynamic_extensions,
            generation,
        )?;
        *self = replacement;
        Ok(())
    }

    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Descriptors in validated dependency order, with linkme entries included
    /// first as the stable tie breaker for otherwise independent entries.
    pub fn extensions(&self) -> impl Iterator<Item = &OwnedExtensionDescriptor> {
        self.ordered_extensions.iter()
    }

    pub fn dynamic_extensions(&self) -> &[OwnedExtensionDescriptor] {
        &self.dynamic_extensions
    }

    pub fn tool_owner(&self, tool: &OwnedToolId) -> Option<&OwnedExtensionId> {
        self.tool_owners.get(tool)
    }

    pub fn tools(&self) -> impl Iterator<Item = (&OwnedExtensionId, &OwnedToolDescriptor)> + '_ {
        self.ordered_extensions.iter().flat_map(|extension| {
            extension
                .tools()
                .iter()
                .map(move |tool| (extension.id(), tool))
        })
    }

    fn build(
        static_extensions: Vec<OwnedExtensionDescriptor>,
        dynamic_extensions: Vec<OwnedExtensionDescriptor>,
        generation: u64,
    ) -> Result<Self, RegistryError> {
        let mut extensions = static_extensions.clone();
        extensions.extend(dynamic_extensions.iter().cloned());
        if extensions.len() > MAX_OWNED_EXTENSIONS {
            return Err(RegistryError::new("owned registry has too many extensions"));
        }

        let mut indexes_by_id = HashMap::new();
        let mut providers = HashMap::<OwnedCapabilityId, usize>::new();
        let mut tool_owners = HashMap::<OwnedToolId, OwnedExtensionId>::new();
        for (index, extension) in extensions.iter().enumerate() {
            if indexes_by_id
                .insert(extension.id().clone(), index)
                .is_some()
            {
                return Err(RegistryError::new(format!(
                    "extension ID `{}` is registered more than once",
                    extension.id()
                )));
            }
            if extension.provides().len() > MAX_OWNED_CAPABILITIES_PER_EXTENSION
                || extension.requires().len() > MAX_OWNED_CAPABILITIES_PER_EXTENSION
                || extension.tools().len() > MAX_OWNED_TOOLS_PER_EXTENSION
            {
                return Err(RegistryError::new(format!(
                    "extension `{}` declares too many tools or capabilities",
                    extension.id()
                )));
            }

            let mut own_providers = BTreeSet::new();
            for capability in extension.provides() {
                validate_owned_identifier(capability.as_str(), "capability")
                    .map_err(|error| RegistryError::new(error.to_string()))?;
                if !own_providers.insert(capability.clone())
                    || providers.insert(capability.clone(), index).is_some()
                {
                    return Err(RegistryError::new(format!(
                        "capability `{capability}` has more than one provider"
                    )));
                }
            }

            let mut own_tools = BTreeSet::new();
            for tool in extension.tools() {
                validate_owned_identifier(tool.id.as_str(), "tool")
                    .map_err(|error| RegistryError::new(error.to_string()))?;
                if tool.description.trim().is_empty()
                    || tool.description.len() > MAX_OWNED_TOOL_DESCRIPTION_BYTES
                {
                    return Err(RegistryError::new(format!(
                        "tool `{}` in extension `{}` has an invalid description",
                        tool.id,
                        extension.id()
                    )));
                }
                if !tool.input_schema.is_object() {
                    return Err(RegistryError::new(format!(
                        "tool `{}` in extension `{}` has a JSON Schema that is not an object",
                        tool.id,
                        extension.id()
                    )));
                }
                let schema_bytes = serde_json::to_vec(&tool.input_schema)
                    .map_err(|_| RegistryError::new("tool schema could not be encoded"))?;
                if schema_bytes.len() > MAX_OWNED_TOOL_SCHEMA_BYTES {
                    return Err(RegistryError::new(format!(
                        "tool `{}` in extension `{}` has an oversized JSON Schema",
                        tool.id,
                        extension.id()
                    )));
                }
                if !own_tools.insert(tool.id.clone())
                    || tool_owners
                        .insert(tool.id.clone(), extension.id().clone())
                        .is_some()
                {
                    return Err(RegistryError::new(format!(
                        "tool ID `{}` is registered more than once",
                        tool.id
                    )));
                }
            }
        }

        let mut dependencies = vec![BTreeSet::<usize>::new(); extensions.len()];
        let mut dependents = vec![Vec::<usize>::new(); extensions.len()];
        for (index, extension) in extensions.iter().enumerate() {
            let mut seen_requirements = BTreeSet::new();
            for required in extension.requires() {
                if !seen_requirements.insert(required.clone()) {
                    return Err(RegistryError::new(format!(
                        "extension `{}` repeats required capability `{required}`",
                        extension.id()
                    )));
                }
                let provider = providers.get(required).ok_or_else(|| {
                    RegistryError::new(format!(
                        "extension `{}` requires missing capability `{required}`",
                        extension.id()
                    ))
                })?;
                if *provider == index {
                    return Err(RegistryError::new(format!(
                        "extension `{}` requires its own capability `{required}`",
                        extension.id()
                    )));
                }
                dependencies[index].insert(*provider);
                dependents[*provider].push(index);
            }
        }

        let mut ready = (0..extensions.len())
            .filter(|index| dependencies[*index].is_empty())
            .collect::<Vec<_>>();
        let mut ordered_extensions = Vec::with_capacity(extensions.len());
        while !ready.is_empty() {
            let index = ready.remove(0);
            ordered_extensions.push(extensions[index].clone());
            for dependent in &dependents[index] {
                dependencies[*dependent].remove(&index);
                if dependencies[*dependent].is_empty() && !ready.contains(dependent) {
                    ready.push(*dependent);
                }
            }
            ready.sort_unstable();
        }
        if ordered_extensions.len() != extensions.len() {
            let cycle = extensions
                .iter()
                .enumerate()
                .filter(|(index, _)| !dependencies[*index].is_empty())
                .map(|(_, extension)| extension.id().as_str())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(RegistryError::new(format!(
                "owned extension dependency cycle detected among: {cycle}"
            )));
        }

        Ok(Self {
            generation,
            static_extensions,
            dynamic_extensions,
            ordered_extensions,
            tool_owners,
        })
    }
}

fn owned_descriptor_from_static(
    descriptor: &ExtensionDescriptor,
) -> Result<OwnedExtensionDescriptor, RegistryError> {
    let tools = descriptor
        .tools
        .iter()
        .map(|tool| {
            let schema = serde_json::from_str(tool.input_schema).map_err(|_| {
                RegistryError::new(format!(
                    "tool `{}` in extension `{}` has invalid JSON Schema JSON",
                    tool.id, descriptor.id
                ))
            })?;
            OwnedToolDescriptor::new(tool.id.as_str(), tool.description, schema)
                .map_err(|error| RegistryError::new(error.to_string()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    OwnedExtensionDescriptor::new(
        descriptor.id.as_str(),
        descriptor
            .provides
            .iter()
            .map(|capability| capability.as_str().to_owned())
            .collect(),
        descriptor
            .requires
            .iter()
            .map(|capability| capability.as_str().to_owned())
            .collect(),
        tools,
    )
    .map_err(|error| RegistryError::new(error.to_string()))
}

#[cfg(test)]
mod owned_registry_tests {
    use super::*;

    struct StaticProbe;

    impl ExtensionHandler for StaticProbe {
        fn call(
            &self,
            _context: &mut dyn ExtensionContext,
            _tool: ToolId,
            _input: Value,
        ) -> Result<Value, ExtensionError> {
            Err(ExtensionError::unsupported(
                "test handler is not dispatched",
            ))
        }
    }

    static STATIC_PROBE: StaticProbe = StaticProbe;
    static STATIC_PROVIDES: [CapabilityId; 1] = [CapabilityId::new("cap.static")];
    static STATIC_TOOLS: [ToolDescriptor; 1] = [ToolDescriptor {
        id: ToolId::new("static.status"),
        description: "Read static status",
        input_schema: r#"{"type":"object"}"#,
    }];
    static STATIC_DESCRIPTOR: ExtensionDescriptor = ExtensionDescriptor {
        id: ExtensionId::new("org.example.static"),
        provides: &STATIC_PROVIDES,
        requires: &[],
        tools: &STATIC_TOOLS,
    };
    static STATIC_REGISTRATION: ExtensionRegistration = ExtensionRegistration {
        descriptor: &STATIC_DESCRIPTOR,
        handler: &STATIC_PROBE,
    };

    fn tool(id: &str) -> OwnedToolDescriptor {
        OwnedToolDescriptor::new(id, "A generic tool", serde_json::json!({"type":"object"}))
            .unwrap()
    }

    fn extension(
        id: &str,
        provides: &[&str],
        requires: &[&str],
        tools: &[&str],
    ) -> OwnedExtensionDescriptor {
        OwnedExtensionDescriptor::new(
            id,
            provides.iter().map(|item| (*item).to_owned()).collect(),
            requires.iter().map(|item| (*item).to_owned()).collect(),
            tools.iter().map(|item| tool(item)).collect(),
        )
        .unwrap()
    }

    #[test]
    fn owned_ids_accept_runtime_strings_and_reject_invalid_identifiers() {
        let runtime_id = format!("org.example.{}", "downloaded");
        assert_eq!(
            OwnedExtensionId::try_new(runtime_id).unwrap().as_str(),
            "org.example.downloaded"
        );
        assert!(OwnedToolId::try_new("tool with spaces").is_err());
        assert!(OwnedCapabilityId::try_new("../outside").is_err());
    }

    #[test]
    fn owned_registry_rejects_duplicate_extension_and_tool_ids() {
        assert!(OwnedExtensionRegistry::new(vec![
            extension("org.example.one", &["cap.one"], &[], &["shared.tool"]),
            extension("org.example.one", &["cap.two"], &[], &["second.tool"]),
        ])
        .is_err());

        assert!(OwnedExtensionRegistry::new(vec![
            extension("org.example.one", &["cap.one"], &[], &["shared.tool"]),
            extension("org.example.two", &["cap.two"], &[], &["shared.tool"]),
        ])
        .is_err());
    }

    #[test]
    fn owned_registry_includes_static_metadata_and_orders_dependencies() {
        let static_registry = ExtensionRegistry::new(vec![&STATIC_REGISTRATION]).unwrap();
        let registry = OwnedExtensionRegistry::with_static_registry(
            &static_registry,
            vec![extension(
                "org.example.dynamic",
                &["cap.dynamic"],
                &["cap.static"],
                &["dynamic.status"],
            )],
        )
        .unwrap();

        let order = registry
            .extensions()
            .map(|entry| entry.id().as_str())
            .collect::<Vec<_>>();
        assert_eq!(order, ["org.example.static", "org.example.dynamic"]);
        let static_tool = OwnedToolId::try_new("static.status").unwrap();
        assert_eq!(
            registry.tool_owner(&static_tool).unwrap().as_str(),
            "org.example.static"
        );
        assert_eq!(registry.tools().count(), 2);
    }

    #[test]
    fn owned_registry_rejects_missing_dependencies_and_cycles() {
        assert!(OwnedExtensionRegistry::new(vec![extension(
            "org.example.orphan",
            &["cap.orphan"],
            &["cap.missing"],
            &[],
        )])
        .is_err());

        assert!(OwnedExtensionRegistry::new(vec![
            extension("org.example.one", &["cap.one"], &["cap.two"], &[]),
            extension("org.example.two", &["cap.two"], &["cap.one"], &[]),
        ])
        .is_err());
    }

    #[test]
    fn replacement_is_validate_before_publish_and_increments_generation() {
        let mut registry = OwnedExtensionRegistry::new(vec![extension(
            "org.example.initial",
            &["cap.initial"],
            &[],
            &["initial.tool"],
        )])
        .unwrap();
        let initial_tool = OwnedToolId::try_new("initial.tool").unwrap();
        let next_tool = OwnedToolId::try_new("next.tool").unwrap();

        assert!(registry
            .replace_dynamic(vec![
                extension("org.example.duplicate", &["cap.one"], &[], &[]),
                extension("org.example.duplicate", &["cap.two"], &[], &[]),
            ])
            .is_err());
        assert_eq!(registry.generation(), 0);
        assert!(registry.tool_owner(&initial_tool).is_some());
        assert!(registry.tool_owner(&next_tool).is_none());

        registry
            .replace_dynamic(vec![extension(
                "org.example.next",
                &["cap.next"],
                &[],
                &["next.tool"],
            )])
            .unwrap();
        assert_eq!(registry.generation(), 1);
        assert!(registry.tool_owner(&initial_tool).is_none());
        assert_eq!(
            registry.tool_owner(&next_tool).unwrap().as_str(),
            "org.example.next"
        );
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    use std::sync::Arc;

    struct Probe {
        name: &'static str,
        log: Arc<Mutex<Vec<String>>>,
        fail_initialize: bool,
        fail_shutdown: bool,
    }

    impl ExtensionHandler for Probe {
        fn initialize(&self) -> Result<(), ExtensionError> {
            self.log.lock().unwrap().push(format!("init:{}", self.name));
            if self.fail_initialize {
                Err(ExtensionError::new(format!("init:{}", self.name)))
            } else {
                Ok(())
            }
        }

        fn shutdown(&self) -> Result<(), ExtensionError> {
            self.log
                .lock()
                .unwrap()
                .push(format!("shutdown:{}", self.name));
            if self.fail_shutdown {
                Err(ExtensionError::new(format!("shutdown:{}", self.name)))
            } else {
                Ok(())
            }
        }

        fn call(
            &self,
            _context: &mut dyn ExtensionContext,
            _tool: ToolId,
            _input: Value,
        ) -> Result<Value, ExtensionError> {
            Err(ExtensionError::unsupported("not used in lifecycle tests"))
        }
    }

    fn registration(
        name: &'static str,
        capability: &'static str,
        requirement: Option<&'static str>,
        log: Arc<Mutex<Vec<String>>>,
        fail_initialize: bool,
        fail_shutdown: bool,
    ) -> &'static ExtensionRegistration {
        let provides: &'static [CapabilityId] =
            Box::leak(vec![CapabilityId::new(capability)].into_boxed_slice());
        let requires: &'static [CapabilityId] = match requirement {
            Some(required) => Box::leak(vec![CapabilityId::new(required)].into_boxed_slice()),
            None => Box::leak(Vec::new().into_boxed_slice()),
        };
        let descriptor = Box::leak(Box::new(ExtensionDescriptor {
            id: ExtensionId::new(name),
            provides,
            requires,
            tools: &[],
        }));
        let handler = Box::leak(Box::new(Probe {
            name,
            log,
            fail_initialize,
            fail_shutdown,
        }));
        Box::leak(Box::new(ExtensionRegistration {
            descriptor,
            handler,
        }))
    }

    #[test]
    fn initializes_once_in_dependency_order_and_shuts_down_in_reverse() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let provider = registration("provider", "cap.provider", None, log.clone(), false, false);
        let dependent = registration(
            "dependent",
            "cap.dependent",
            Some("cap.provider"),
            log.clone(),
            false,
            false,
        );
        let registry = ExtensionRegistry::new(vec![dependent, provider]).unwrap();

        registry.initialize().unwrap();
        registry.initialize().unwrap();
        registry.shutdown().unwrap();
        registry.shutdown().unwrap();

        assert_eq!(
            *log.lock().unwrap(),
            [
                "init:provider",
                "init:dependent",
                "shutdown:dependent",
                "shutdown:provider",
            ]
        );
    }

    #[test]
    fn failed_initialization_rolls_back_and_is_terminal() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let provider = registration("provider", "cap.provider", None, log.clone(), false, false);
        let dependent = registration(
            "dependent",
            "cap.dependent",
            Some("cap.provider"),
            log.clone(),
            true,
            false,
        );
        let registry = ExtensionRegistry::new(vec![dependent, provider]).unwrap();

        let error = registry.initialize().unwrap_err();
        assert_eq!(error.message(), "init:dependent");
        assert_eq!(
            *log.lock().unwrap(),
            ["init:provider", "init:dependent", "shutdown:provider"]
        );

        assert_eq!(
            registry.initialize().unwrap_err().message(),
            "init:dependent"
        );
        registry.shutdown().unwrap();
        assert_eq!(
            *log.lock().unwrap(),
            ["init:provider", "init:dependent", "shutdown:provider"]
        );
    }

    #[test]
    fn shutdown_attempts_every_hook_and_caches_first_error() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let provider = registration("provider", "cap.provider", None, log.clone(), false, true);
        let dependent = registration(
            "dependent",
            "cap.dependent",
            Some("cap.provider"),
            log.clone(),
            false,
            true,
        );
        let registry = ExtensionRegistry::new(vec![dependent, provider]).unwrap();
        registry.initialize().unwrap();

        let error = registry.shutdown().unwrap_err();
        assert_eq!(error.message(), "shutdown:dependent");
        assert_eq!(
            *log.lock().unwrap(),
            [
                "init:provider",
                "init:dependent",
                "shutdown:dependent",
                "shutdown:provider",
            ]
        );

        assert_eq!(
            registry.shutdown().unwrap_err().message(),
            "shutdown:dependent"
        );
        assert_eq!(log.lock().unwrap().len(), 4);
    }

    #[test]
    fn shutdown_before_initialization_prevents_later_initialization() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let extension = registration(
            "extension",
            "cap.extension",
            None,
            log.clone(),
            false,
            false,
        );
        let registry = ExtensionRegistry::new(vec![extension]).unwrap();

        registry.shutdown().unwrap();
        let error = registry.initialize().unwrap_err();

        assert_eq!(error.kind(), ExtensionErrorKind::Conflict);
        assert!(log.lock().unwrap().is_empty());
    }
}

#[cfg(test)]
mod invocation_security_tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn capability_operation_logs_use_a_bounded_label_not_caller_text() {
        let caller_supplied_operation = "sentinel-user-payload-".repeat(512);
        let capability = CapabilityId::new("audit.safe.capability");
        let (operation_label, capability_label) =
            logged_operation_fields(LoggedOperation::Capability(capability));

        assert_eq!(operation_label, "capability");
        assert_eq!(capability_label, "audit.safe.capability");
        assert_eq!(
            capability_operation_label(&caller_supplied_operation),
            "capability"
        );
        assert!(caller_supplied_operation.len() > 10_000);
    }

    #[test]
    fn default_capability_error_does_not_echo_caller_operation() {
        struct DefaultInvoke;
        impl ExtensionHandler for DefaultInvoke {
            fn call(
                &self,
                _context: &mut dyn ExtensionContext,
                _tool: ToolId,
                _input: Value,
            ) -> Result<Value, ExtensionError> {
                Ok(Value::Null)
            }
        }

        let sentinel = "sensitive-request-payload".repeat(128);
        let mut context = host_context(Arc::new(Mutex::new(Vec::new())));
        let error = DefaultInvoke
            .invoke(
                &mut context,
                CapabilityId::new("audit.safe.capability"),
                &sentinel,
                Value::Null,
            )
            .unwrap_err();

        assert!(!error.message().contains(&sentinel));
        assert_eq!(
            error.message(),
            "extension does not implement the requested capability operation"
        );
    }

    #[derive(Default)]
    struct EmptyDocuments;
    impl ScopedDocumentStore for EmptyDocuments {
        fn get(&self, _id: &str) -> Result<Option<ExtensionDocument>, ExtensionError> {
            Ok(None)
        }
        fn list(&self, _kind: Option<&str>) -> Result<Vec<ExtensionDocument>, ExtensionError> {
            Ok(Vec::new())
        }
        fn put_batch(&mut self, _documents: Vec<ExtensionDocument>) -> Result<(), ExtensionError> {
            Ok(())
        }
    }

    struct HostContext {
        documents: EmptyDocuments,
        provider: String,
        scopes: Arc<Mutex<Vec<String>>>,
    }
    impl ExtensionContext for HostContext {
        fn owner(&self) -> &str {
            "owner-fixed"
        }
        fn workspace(&self) -> &str {
            "workspace-fixed"
        }
        fn documents(&mut self) -> &mut dyn ScopedDocumentStore {
            &mut self.documents
        }
        fn scoped_documents(&mut self, scope: &ExtensionScope) -> &mut dyn ScopedDocumentStore {
            self.scopes
                .lock()
                .unwrap()
                .push(scope.extension_id().as_str().to_owned());
            &mut self.documents
        }
        fn invocation_identity(&self) -> InvocationIdentity<'_> {
            InvocationIdentity {
                provider: &self.provider,
                principal: None,
                caller_extension: None,
            }
        }
        fn invoke(
            &mut self,
            _capability: CapabilityId,
            _operation: &str,
            _input: Value,
        ) -> Result<Value, ExtensionError> {
            Err(ExtensionError::new(
                "host context must not dispatch directly",
            ))
        }
    }

    #[derive(Clone, Copy)]
    enum ProbeAction {
        Call(CapabilityId),
        ObserveAndCall(CapabilityId),
        Observe,
    }
    struct Probe {
        action: ProbeAction,
        callers: Arc<Mutex<Vec<Option<ExtensionId>>>>,
        routes: &'static [ExtensionRouteDescriptor],
    }
    impl ExtensionHandler for Probe {
        fn http_routes(&self) -> &'static [ExtensionRouteDescriptor] {
            self.routes
        }
        fn call(
            &self,
            context: &mut dyn ExtensionContext,
            _tool: ToolId,
            _input: Value,
        ) -> Result<Value, ExtensionError> {
            context.documents().list(None)?;
            match self.action {
                ProbeAction::Call(capability) => context.invoke(capability, "probe", Value::Null),
                _ => Ok(Value::Null),
            }
        }
        fn invoke(
            &self,
            context: &mut dyn ExtensionContext,
            _capability: CapabilityId,
            _operation: &str,
            _input: Value,
        ) -> Result<Value, ExtensionError> {
            self.callers
                .lock()
                .unwrap()
                .push(context.invocation_identity().caller_extension);
            context.documents().list(None)?;
            match self.action {
                ProbeAction::ObserveAndCall(capability) => {
                    context.invoke(capability, "probe", Value::Null)
                }
                ProbeAction::Observe => Ok(Value::Null),
                ProbeAction::Call(capability) => context.invoke(capability, "probe", Value::Null),
            }
        }

        fn handle_http(
            &self,
            context: &mut dyn ExtensionContext,
            _route: RouteId,
            _request: ExtensionHttpRequest,
        ) -> Result<ExtensionHttpResponse, ExtensionError> {
            self.callers
                .lock()
                .unwrap()
                .push(context.invocation_identity().caller_extension);
            context.documents().list(None)?;
            Ok(ExtensionHttpResponse::json(200, Value::Null))
        }
    }

    fn register(
        id: &'static str,
        provides: &[&'static str],
        requires: &[&'static str],
        tool: Option<&'static str>,
        action: ProbeAction,
        callers: Arc<Mutex<Vec<Option<ExtensionId>>>>,
        _scopes: Arc<Mutex<Vec<String>>>,
    ) -> &'static ExtensionRegistration {
        let mut provides = provides
            .iter()
            .map(|id| CapabilityId::new(id))
            .collect::<Vec<_>>();
        if provides.is_empty() {
            provides.push(CapabilityId::new(match id {
                "deny-caller" => "deny.caller.self",
                "frame-a" => "frame.cap.a",
                "serial-probe" => "serial.cap",
                _ => "probe.self",
            }));
        }
        let provides = Box::leak(provides.into_boxed_slice());
        let requires = Box::leak(
            requires
                .iter()
                .map(|id| CapabilityId::new(id))
                .collect::<Vec<_>>()
                .into_boxed_slice(),
        );
        let tools = tool
            .map(|id| {
                Box::leak(
                    vec![ToolDescriptor {
                        id: ToolId::new(id),
                        description: "probe",
                        input_schema: r#"{"type":"object"}"#,
                    }]
                    .into_boxed_slice(),
                ) as &'static [ToolDescriptor]
            })
            .unwrap_or(&[]);
        let descriptor = Box::leak(Box::new(ExtensionDescriptor {
            id: ExtensionId::new(id),
            provides,
            requires,
            tools,
        }));
        let handler = Box::leak(Box::new(Probe {
            action,
            callers,
            routes: &[],
        }));
        Box::leak(Box::new(ExtensionRegistration {
            descriptor,
            handler,
        }))
    }

    fn host_context(scopes: Arc<Mutex<Vec<String>>>) -> HostContext {
        HostContext {
            documents: EmptyDocuments,
            provider: "trusted-transport".into(),
            scopes,
        }
    }

    #[test]
    fn nested_calls_require_both_declared_and_host_approved_grants() {
        for (required, approved) in [(false, true), (true, false)] {
            let callers = Arc::new(Mutex::new(Vec::new()));
            let scopes = Arc::new(Mutex::new(Vec::new()));
            let caller = register(
                "deny-caller",
                &[],
                if required { &["deny.cap"] } else { &[] },
                Some("deny.tool"),
                ProbeAction::Call(CapabilityId::new("deny.cap")),
                callers.clone(),
                scopes.clone(),
            );
            let provider = register(
                "deny-provider",
                &["deny.cap"],
                &[],
                None,
                ProbeAction::Observe,
                callers.clone(),
                scopes.clone(),
            );
            let approved = if approved {
                HashMap::from([(
                    ExtensionId::new("deny-caller"),
                    BTreeSet::from([CapabilityId::new("deny.cap")]),
                )])
            } else {
                HashMap::new()
            };
            let registry =
                ExtensionRegistry::new_with_approved_capabilities(vec![caller, provider], approved)
                    .unwrap();
            let mut context = host_context(scopes);
            let error = registry
                .call_named(&mut context, "deny.tool", Value::Null)
                .unwrap_err();
            assert_eq!(error.kind(), ExtensionErrorKind::Forbidden);
        }
    }

    #[test]
    fn context_free_invoke_is_denied_and_host_entry_is_explicit() {
        let callers = Arc::new(Mutex::new(Vec::new()));
        let scopes = Arc::new(Mutex::new(Vec::new()));
        let provider = register(
            "host-entry-provider",
            &["host.entry.cap"],
            &[],
            None,
            ProbeAction::Observe,
            callers.clone(),
            scopes.clone(),
        );
        let registry = ExtensionRegistry::new(vec![provider]).unwrap();

        let mut context = host_context(scopes.clone());
        let error = registry
            .invoke(
                &mut context,
                CapabilityId::new("host.entry.cap"),
                "probe",
                Value::Null,
            )
            .unwrap_err();
        assert_eq!(error.kind(), ExtensionErrorKind::Forbidden);
        assert!(scopes.lock().unwrap().is_empty());

        registry
            .invoke_from_host(
                &mut context,
                CapabilityId::new("host.entry.cap"),
                "probe",
                Value::Null,
            )
            .unwrap();
        assert_eq!(*callers.lock().unwrap(), [None]);
        assert_eq!(*scopes.lock().unwrap(), ["host-entry-provider"]);
    }

    #[test]
    fn caller_identity_and_document_namespace_change_at_every_hop() {
        let callers = Arc::new(Mutex::new(Vec::new()));
        let scopes = Arc::new(Mutex::new(Vec::new()));
        let a = register(
            "frame-a",
            &[],
            &["frame.cap.b"],
            Some("frame.tool"),
            ProbeAction::Call(CapabilityId::new("frame.cap.b")),
            callers.clone(),
            scopes.clone(),
        );
        let b = register(
            "frame-b",
            &["frame.cap.b"],
            &["frame.cap.c"],
            None,
            ProbeAction::ObserveAndCall(CapabilityId::new("frame.cap.c")),
            callers.clone(),
            scopes.clone(),
        );
        let c = register(
            "frame-c",
            &["frame.cap.c"],
            &[],
            None,
            ProbeAction::Observe,
            callers.clone(),
            scopes.clone(),
        );
        let approved = HashMap::from([
            (
                ExtensionId::new("frame-a"),
                BTreeSet::from([CapabilityId::new("frame.cap.b")]),
            ),
            (
                ExtensionId::new("frame-b"),
                BTreeSet::from([CapabilityId::new("frame.cap.c")]),
            ),
        ]);
        let registry =
            ExtensionRegistry::new_with_approved_capabilities(vec![a, b, c], approved).unwrap();
        let mut context = host_context(scopes.clone());
        registry
            .call_named(&mut context, "frame.tool", Value::Null)
            .unwrap();
        assert_eq!(
            *callers.lock().unwrap(),
            [
                Some(ExtensionId::new("frame-a")),
                Some(ExtensionId::new("frame-b"))
            ]
        );
        assert_eq!(*scopes.lock().unwrap(), ["frame-a", "frame-b", "frame-c"]);
    }

    #[test]
    fn extension_tool_mutability_metadata_cannot_opt_out_of_serialization() {
        let callers = Arc::new(Mutex::new(Vec::new()));
        let scopes = Arc::new(Mutex::new(Vec::new()));
        let registration = register(
            "serial-probe",
            &[],
            &[],
            Some("serial.tool"),
            ProbeAction::Observe,
            callers,
            scopes,
        );
        let registry = ExtensionRegistry::new(vec![registration]).unwrap();
        assert_eq!(registry.tool_is_mutating_name("serial.tool"), Some(true));
    }

    #[test]
    fn http_route_dispatch_uses_registry_created_identity_and_document_scope() {
        let callers = Arc::new(Mutex::new(Vec::new()));
        let scopes = Arc::new(Mutex::new(Vec::new()));
        let route = Box::leak(Box::new(ExtensionRouteDescriptor {
            id: RouteId::new("route.probe"),
            method: HttpMethod::Get,
            path: "/probe",
            mutates: false,
        }));
        let routes = Box::leak(vec![*route].into_boxed_slice());
        let descriptor = Box::leak(Box::new(ExtensionDescriptor {
            id: ExtensionId::new("route-probe"),
            provides: Box::leak(vec![CapabilityId::new("route.probe.cap")].into_boxed_slice()),
            requires: &[],
            tools: &[],
        }));
        let handler = Box::leak(Box::new(Probe {
            action: ProbeAction::Observe,
            callers: callers.clone(),
            routes,
        }));
        let registration = Box::leak(Box::new(ExtensionRegistration {
            descriptor,
            handler,
        }));
        let registry = ExtensionRegistry::new(vec![registration]).unwrap();
        let matched = registry
            .match_http_route(HttpMethod::Get, "/probe")
            .unwrap();
        assert!(!matched.mutates());
        assert!(matched.requires_serialization());
        let mut context = host_context(scopes.clone());
        let response = registry
            .call_http_route(
                &mut context,
                &matched,
                ExtensionHttpRequest {
                    path_params: Default::default(),
                    query: Default::default(),
                    body: Value::Null,
                },
            )
            .unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(*callers.lock().unwrap(), [None]);
        assert_eq!(*scopes.lock().unwrap(), ["route-probe"]);
    }
}
