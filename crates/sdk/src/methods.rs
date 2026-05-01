//! Typed dispatch — one Rust fn per RPC method.
//!
//! Each fn takes the protocol's typed param struct, sends the request through
//! [`crate::Broker::call_typed`], and returns the protocol's typed result
//! struct. Wire-format compatibility is unchanged — these are thin wrappers
//! over the existing untyped `Broker::call`.
//!
//! Naming mirrors the JSON-RPC method: `<group>.<verb>` becomes
//! `<group>_<verb>` (e.g. `logs.recent` → `logs_recent`).

use crate::{Broker, BrokerError};
use logmon_broker_protocol::{
    BookmarksAdd, BookmarksAddResult, BookmarksClear, BookmarksClearResult, BookmarksList,
    BookmarksListResult, BookmarksRemove, BookmarksRemoveResult, FiltersAdd, FiltersAddResult,
    FiltersEdit, FiltersEditResult, FiltersList, FiltersListResult, FiltersRemove,
    FiltersRemoveResult, LogsClear, LogsClearResult, LogsContext, LogsContextResult, LogsExport,
    LogsExportResult, LogsRecent, LogsRecentResult, SessionDrop, SessionDropResult, SessionList,
    SessionListResult, SpansContext, SpansContextResult, StatusGet, StatusGetResult, TracesGet,
    TracesGetResult, TracesLogs, TracesLogsResult, TracesRecent, TracesRecentResult, TracesSlow,
    TracesSlowResult, TracesSummary, TracesSummaryResult, TriggersAdd, TriggersAddResult,
    TriggersEdit, TriggersEditResult, TriggersList, TriggersListResult, TriggersRemove,
    TriggersRemoveResult,
};

impl Broker {
    // ---- logs.* ----
    pub async fn logs_recent(&self, p: LogsRecent) -> Result<LogsRecentResult, BrokerError> {
        self.call_typed("logs.recent", p).await
    }
    pub async fn logs_context(&self, p: LogsContext) -> Result<LogsContextResult, BrokerError> {
        self.call_typed("logs.context", p).await
    }
    pub async fn logs_export(&self, p: LogsExport) -> Result<LogsExportResult, BrokerError> {
        self.call_typed("logs.export", p).await
    }
    pub async fn logs_clear(&self, p: LogsClear) -> Result<LogsClearResult, BrokerError> {
        self.call_typed("logs.clear", p).await
    }

    // ---- filters.* ----
    pub async fn filters_list(&self, p: FiltersList) -> Result<FiltersListResult, BrokerError> {
        self.call_typed("filters.list", p).await
    }
    pub async fn filters_add(&self, p: FiltersAdd) -> Result<FiltersAddResult, BrokerError> {
        self.call_typed("filters.add", p).await
    }
    pub async fn filters_edit(&self, p: FiltersEdit) -> Result<FiltersEditResult, BrokerError> {
        self.call_typed("filters.edit", p).await
    }
    pub async fn filters_remove(
        &self,
        p: FiltersRemove,
    ) -> Result<FiltersRemoveResult, BrokerError> {
        self.call_typed("filters.remove", p).await
    }

    // ---- triggers.* ----
    pub async fn triggers_list(&self, p: TriggersList) -> Result<TriggersListResult, BrokerError> {
        self.call_typed("triggers.list", p).await
    }
    pub async fn triggers_add(&self, p: TriggersAdd) -> Result<TriggersAddResult, BrokerError> {
        self.call_typed("triggers.add", p).await
    }
    pub async fn triggers_edit(&self, p: TriggersEdit) -> Result<TriggersEditResult, BrokerError> {
        self.call_typed("triggers.edit", p).await
    }
    pub async fn triggers_remove(
        &self,
        p: TriggersRemove,
    ) -> Result<TriggersRemoveResult, BrokerError> {
        self.call_typed("triggers.remove", p).await
    }

    // ---- traces.* / spans.* ----
    pub async fn traces_recent(&self, p: TracesRecent) -> Result<TracesRecentResult, BrokerError> {
        self.call_typed("traces.recent", p).await
    }
    pub async fn traces_get(&self, p: TracesGet) -> Result<TracesGetResult, BrokerError> {
        self.call_typed("traces.get", p).await
    }
    pub async fn traces_summary(
        &self,
        p: TracesSummary,
    ) -> Result<TracesSummaryResult, BrokerError> {
        self.call_typed("traces.summary", p).await
    }
    pub async fn traces_slow(&self, p: TracesSlow) -> Result<TracesSlowResult, BrokerError> {
        self.call_typed("traces.slow", p).await
    }
    pub async fn traces_logs(&self, p: TracesLogs) -> Result<TracesLogsResult, BrokerError> {
        self.call_typed("traces.logs", p).await
    }
    pub async fn spans_context(&self, p: SpansContext) -> Result<SpansContextResult, BrokerError> {
        self.call_typed("spans.context", p).await
    }

    // ---- bookmarks.* ----
    pub async fn bookmarks_add(&self, p: BookmarksAdd) -> Result<BookmarksAddResult, BrokerError> {
        self.call_typed("bookmarks.add", p).await
    }
    pub async fn bookmarks_list(
        &self,
        p: BookmarksList,
    ) -> Result<BookmarksListResult, BrokerError> {
        self.call_typed("bookmarks.list", p).await
    }
    pub async fn bookmarks_remove(
        &self,
        p: BookmarksRemove,
    ) -> Result<BookmarksRemoveResult, BrokerError> {
        self.call_typed("bookmarks.remove", p).await
    }
    pub async fn bookmarks_clear(
        &self,
        p: BookmarksClear,
    ) -> Result<BookmarksClearResult, BrokerError> {
        self.call_typed("bookmarks.clear", p).await
    }

    // ---- session.* / status.* ----
    pub async fn session_list(&self, p: SessionList) -> Result<SessionListResult, BrokerError> {
        self.call_typed("session.list", p).await
    }
    pub async fn session_drop(&self, p: SessionDrop) -> Result<SessionDropResult, BrokerError> {
        self.call_typed("session.drop", p).await
    }
    pub async fn status_get(&self, p: StatusGet) -> Result<StatusGetResult, BrokerError> {
        self.call_typed("status.get", p).await
    }
}
