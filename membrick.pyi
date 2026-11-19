"""Typed public API for the Membrick Python extension."""

from typing import Any, Dict, List, Literal, Mapping, Optional, Sequence, TypedDict, Union, overload


class MembrickError(Exception):
    """Base class for errors returned by the Membrick client."""


class RejectedError(ValueError, MembrickError):
    """The service definitively rejected the operation."""
    status: int


class ConfigurationError(ValueError, MembrickError):
    """The client configuration is invalid."""


class UnavailableError(ConnectionError, MembrickError):
    """The service could not be reached before dispatch."""


class UncertainError(RuntimeError, MembrickError):
    """The operation may have reached the service; do not blindly replay it."""


class MemorySearchResponse(TypedDict):
    search_id: str
    data: List[Dict[str, Any]]


class SearchReviewResponse(TypedDict):
    search_id: str
    reviewed: bool
    search: Dict[str, Any]
    result: Dict[str, Any]
    documents: List[Dict[str, Any]]
    prior_feedback: Optional[Any]


class SearchReviewPageEntry(TypedDict):
    search_id: str
    search: Dict[str, Any]
    result: Dict[str, Any]
    documents: List[Dict[str, Any]]


class SearchReviewPage(TypedDict):
    searches: List[SearchReviewPageEntry]
    next_cursor: Optional[str]
    has_more: bool
    oversized_records_skipped: int
    malformed_records_skipped: int
    first_malformed_offset: Optional[int]
    ambiguous_searches_skipped: int


class SearchReviewExactIndexing(TypedDict):
    status: Literal["indexing"]
    search_id: str
    indexed_bytes: int
    history_bytes: int
    message: str


class SearchReviewPageIndexing(TypedDict):
    status: Literal["indexing"]
    searches: List[Any]
    indexed_bytes: int
    history_bytes: int
    message: str


class SearchReviewHistoryIncomplete(TypedDict):
    status: Literal["history_incomplete"]
    search_id: str
    malformed_records_skipped: int
    oversized_records_skipped: int
    first_malformed_offset: Optional[int]
    message: str


ReviewSearchResult = Union[
    SearchReviewResponse,
    SearchReviewPage,
    SearchReviewExactIndexing,
    SearchReviewPageIndexing,
    SearchReviewHistoryIncomplete,
]


class Memory:
    """Connect to a project resident or an explicitly configured HTTP service."""

    def __init__(
        self,
        path: Optional[str] = ...,
        base_url: Optional[str] = ...,
        api_key: Optional[str] = ...,
        request_timeout_ms: int = ...,
        resident_executable: Optional[str] = ...,
    ) -> None: ...

    @staticmethod
    def init(
        path: str,
        api_key: Optional[str] = ...,
        request_timeout_ms: int = ...,
        resident_executable: Optional[str] = ...,
    ) -> Memory: ...

    def add(
        self,
        messages: Union[str, Mapping[str, Any], Sequence[Mapping[str, Any]]],
        user_id: str,
        request_id: Optional[str] = ...,
        session_id: Optional[str] = ...,
        wait_for_visibility: bool = ...,
    ) -> Dict[str, Any]: ...

    def add_transaction(
        self, requests: Sequence[Mapping[str, Any]]
    ) -> List[Dict[str, Any]]: ...

    def search(
        self,
        query: str,
        user_id: str,
        top_k: Optional[int] = ...,
        session_id: Optional[str] = ...,
        filters: Optional[Mapping[str, str]] = ...,
        scope: Optional[str] = ...,
        reference_date: Optional[str] = ...,
        options: Optional[Sequence[str]] = ...,
    ) -> MemorySearchResponse: ...

    def workspace_search(
        self,
        query: str,
        provider: str,
        top_k: int = ...,
        workspace: Optional[str] = ...,
        session_id: Optional[str] = ...,
    ) -> Dict[str, Any]: ...

    @overload
    def review_search(
        self,
        search_id: str,
        user_id: str,
        provider: None = ...,
        workspace: None = ...,
        session_id: None = ...,
        cursor: None = ...,
        page_size: None = ...,
    ) -> SearchReviewResponse: ...

    @overload
    def review_search(
        self,
        search_id: Optional[str] = ...,
        user_id: None = ...,
        provider: Optional[str] = ...,
        workspace: Optional[str] = ...,
        session_id: Optional[str] = ...,
        cursor: Optional[str] = ...,
        page_size: Optional[int] = ...,
    ) -> ReviewSearchResult: ...

    def get(
        self, memory_id: str, user_id: str, include_inactive: bool = ...
    ) -> Optional[Dict[str, Any]]: ...

    def list(
        self,
        user_id: str,
        session_id: Optional[str] = ...,
        limit: int = ...,
        cursor: Optional[str] = ...,
        include_inactive: bool = ...,
    ) -> Dict[str, Any]: ...

    def update(
        self,
        memory_id: str,
        user_id: str,
        content: str,
        role: Optional[str] = ...,
        timestamp: Optional[int] = ...,
        expires_at_ms: Optional[int] = ...,
    ) -> Optional[Dict[str, Any]]: ...

    def delete(self, memory_id: str, user_id: str) -> bool: ...

    def refresh(self) -> None: ...

    def close(self) -> None: ...

    def session(self, user_id: str, session_id: str) -> "MemorySession": ...


class MemorySession:
    @property
    def user_id(self) -> str: ...

    @property
    def session_id(self) -> str: ...

    def close(self) -> None: ...

    def add(
        self,
        messages: Union[str, Mapping[str, Any], Sequence[Mapping[str, Any]]],
        request_id: Optional[str] = ...,
        wait_for_visibility: bool = ...,
    ) -> Dict[str, Any]: ...

    def search(
        self,
        query: str,
        top_k: Optional[int] = ...,
        filters: Optional[Mapping[str, str]] = ...,
        scope: Optional[str] = ...,
        reference_date: Optional[str] = ...,
        options: Optional[Sequence[str]] = ...,
    ) -> MemorySearchResponse: ...

    def review_search(self, search_id: str) -> SearchReviewResponse: ...


class Mem0Adapter:
    """Documented Mem0-style subset; this is not a drop-in Mem0 replacement."""

    def __init__(self, client: Memory) -> None: ...

    def close(self) -> None: ...

    def add(
        self,
        messages: Union[str, Mapping[str, Any], Sequence[Mapping[str, Any]]],
        user_id: str,
        run_id: Optional[str] = ...,
        agent_id: Optional[str] = ...,
        app_id: Optional[str] = ...,
        metadata: Optional[Mapping[str, Any]] = ...,
        infer: Optional[bool] = ...,
        request_id: Optional[str] = ...,
        expiration_date: Optional[str] = ...,
    ) -> Dict[str, Any]: ...

    def search(
        self,
        query: str,
        user_id: Optional[str] = ...,
        top_k: Optional[int] = ...,
        filters: Optional[Mapping[str, Any]] = ...,
        run_id: Optional[str] = ...,
        agent_id: Optional[str] = ...,
        app_id: Optional[str] = ...,
        threshold: Optional[float] = ...,
        rerank: Optional[bool] = ...,
        show_expired: bool = ...,
        limit: Optional[int] = ...,
    ) -> Dict[str, Any]: ...

    def get_all(
        self,
        filters: Optional[Mapping[str, Any]] = ...,
        page: int = ...,
        page_size: int = ...,
        show_expired: bool = ...,
    ) -> Dict[str, Any]: ...

    def get(self, memory_id: str, user_id: str) -> Dict[str, Any]: ...

    @overload
    def update(
        self,
        memory_id: str,
        text: str,
        user_id: str,
        metadata: Optional[Mapping[str, Any]] = ...,
    ) -> Dict[str, Any]: ...

    @overload
    def update(
        self,
        memory_id: str,
        *,
        data: str,
        user_id: str,
        metadata: Optional[Mapping[str, Any]] = ...,
    ) -> Dict[str, Any]: ...

    def delete(self, memory_id: str, user_id: str) -> Dict[str, Any]: ...


def version() -> str: ...
