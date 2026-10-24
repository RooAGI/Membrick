//! Agent bulletin board operations (see crate::board).

use super::*;

fn normalize_board_links(
    links: &[crate::board::BoardLink],
) -> anyhow::Result<Vec<crate::board::BoardLink>> {
    if links.len() > 100 {
        anyhow::bail!("a board item may have at most 100 links");
    }
    links
        .iter()
        .map(|link| {
            let target = link.target.trim();
            let relation = link.relation.trim();
            if target.is_empty()
                || target.chars().count() > 2048
                || relation.is_empty()
                || relation.chars().count() > 64
            {
                anyhow::bail!(
                    "link targets must be 1-2048 characters and relations 1-64 characters"
                );
            }
            Ok(crate::board::BoardLink {
                target: target.to_string(),
                relation: relation.to_string(),
            })
        })
        .collect()
}

impl MemoryService {
    // -----------------------------------------------------------------------
    // Agent bulletin board (see crate::board).
    // -----------------------------------------------------------------------

    /// Resolve an optional `board_id` to a concrete board ID.
    ///
    /// `None` (or empty, or the `"default"` alias) means the current
    /// session's board: the default board *points to* the board whose ID
    /// is derived from the session ID. `session_id` must resolve when the
    /// default is used, otherwise this errors — there is no silent
    /// fallback to a shared board. When `ensure_default` is set, the
    /// session board's persistent record is created if missing — used by
    /// `board_post` so the first post creates it under the writer path.
    /// Reads pass false and treat a never-created session board as empty.
    fn resolve_board_id(
        &mut self,
        board_id: Option<&str>,
        owner: &str,
        workspace: &str,
        session_id: Option<&str>,
        ensure_default: bool,
    ) -> anyhow::Result<String> {
        use crate::board::*;
        let trimmed = board_id.map(str::trim).filter(|s| !s.is_empty());
        // The "default" alias resolves exactly like an omitted board_id.
        let trimmed = match trimmed {
            Some(id) if is_default_alias(id) => None,
            other => other,
        };
        match trimmed {
            Some(id) => {
                if ensure_default {
                    if let Some(sid) = session_id.map(str::trim).filter(|s| !s.is_empty()) {
                        if is_default_board(id, owner, workspace, sid) {
                            self.board_open_session(owner, workspace, sid, DEFAULT_BOARD_TITLE)?;
                        }
                    }
                }
                if self.board_info(id, owner, workspace, session_id)?.is_none() {
                    anyhow::bail!("unknown board_id: {id}");
                }
                Ok(id.to_string())
            }
            None => {
                let sid = session_id
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "no session_id: pass session_id (or a board_id) to use the default board"
                        )
                    })?;
                if ensure_default {
                    let board =
                        self.board_open_session(owner, workspace, sid, DEFAULT_BOARD_TITLE)?;
                    Ok(board.board_id)
                } else {
                    Ok(default_board_id(owner, workspace, sid))
                }
            }
        }
    }

    /// Idempotently open a board for (owner, workspace, key).
    ///
    /// Returns the existing board when the key is already taken; otherwise
    /// creates it. The board ID is deterministic for the triple, so
    /// concurrent opens converge without coordination.
    ///
    /// The key `"default"` is an alias for the current session's board —
    /// use [`Self::board_open_session`] for it (the MCP layer redirects).
    /// Keys starting with `"session:"` are rejected: that namespace is
    /// reserved for session boards.
    pub fn board_open(
        &mut self,
        owner: &str,
        workspace: &str,
        key: &str,
        title: &str,
    ) -> anyhow::Result<crate::board::Board> {
        self.board_open_typed(
            owner,
            workspace,
            key,
            title,
            crate::board::DEFAULT_BOARD_TYPE,
            &[],
        )
    }

    pub fn board_open_typed(
        &mut self,
        owner: &str,
        workspace: &str,
        key: &str,
        title: &str,
        board_type: &str,
        links: &[crate::board::BoardLink],
    ) -> anyhow::Result<crate::board::Board> {
        self.board_open_instance(owner, workspace, key, title, board_type, links, None, None)
    }

    pub fn board_open_instance(
        &mut self,
        owner: &str,
        workspace: &str,
        key: &str,
        title: &str,
        board_type: &str,
        links: &[crate::board::BoardLink],
        template_instance: Option<&crate::board::TemplateInstance>,
        fields: Option<&serde_json::Value>,
    ) -> anyhow::Result<crate::board::Board> {
        use crate::board::*;
        if key.trim().is_empty() {
            anyhow::bail!("board key must not be empty");
        }
        let board_type = board_type.trim();
        if board_type.is_empty()
            || board_type.len() > 64
            || !board_type
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            anyhow::bail!("board type must be 1-64 letters, numbers, '-' or '_' characters");
        }
        let links = normalize_board_links(links)?;
        self.validate_board_links(&links, owner, workspace)?;
        if let Some(instance) = template_instance {
            validate_template_instance(instance)?;
            if let Some(fields) = fields {
                validate_template_fields(&instance.snapshot.schema, fields)?;
            }
        } else if fields.is_some() {
            anyhow::bail!("board fields require a template_instance");
        }
        if key.starts_with(SESSION_BOARD_KEY_PREFIX) {
            anyhow::bail!(
                "board key namespace {SESSION_BOARD_KEY_PREFIX:?} is reserved for session boards"
            );
        }
        if is_default_alias(key) {
            anyhow::bail!(
                "board key \"default\" is reserved: it aliases the current session's board; use board_open_session"
            );
        }
        self.board_open_inner(
            owner,
            workspace,
            key,
            title,
            board_type,
            &links,
            template_instance,
            fields,
        )
    }

    fn validate_board_links(
        &mut self,
        links: &[crate::board::BoardLink],
        owner: &str,
        workspace: &str,
    ) -> anyhow::Result<()> {
        if links.len() > 100 {
            anyhow::bail!("a board item may have at most 100 links");
        }
        for link in links {
            let target = link.target.trim();
            let relation = link.relation.trim();
            if target.is_empty()
                || target.len() > 2048
                || relation.is_empty()
                || relation.len() > 64
            {
                anyhow::bail!(
                    "link targets must be 1-2048 characters and relations 1-64 characters"
                );
            }
            if let Some((target_board, _)) = target.split_once(":post:") {
                let board = self
                    .board_info(target_board, owner, workspace, None)?
                    .ok_or_else(|| anyhow::anyhow!("linked post belongs to an unknown board"))?;
                if self
                    .board_get(Some(&board.board_id), owner, workspace, None, target)?
                    .is_none()
                {
                    anyhow::bail!("linked post does not exist in this workspace: {target}");
                }
            } else if target.starts_with("board:") {
                if self.board_info(target, owner, workspace, None)?.is_none() {
                    anyhow::bail!("linked board does not exist in this workspace: {target}");
                }
            } else if target.starts_with("https://") {
                let parsed = reqwest::Url::parse(target)
                    .map_err(|_| anyhow::anyhow!("link target is not a valid https URL"))?;
                if parsed.scheme() != "https" || parsed.host_str().is_none() {
                    anyhow::bail!("link target must be an https URL with a host");
                }
            } else if target
                .strip_prefix("repo:")
                .is_some_and(|reference| !reference.trim().is_empty())
            {
                // Repository references remain generic identifiers. The
                // dashboard displays them as references without interpreting
                // them as local filesystem paths.
            } else {
                anyhow::bail!(
                    "link target must be a workspace board/post ID, https URL, or repo: reference"
                );
            }
        }
        Ok(())
    }

    /// Inner open without the reserved-namespace guard; session boards
    /// are created through this path.
    fn board_open_inner(
        &mut self,
        owner: &str,
        workspace: &str,
        key: &str,
        title: &str,
        board_type: &str,
        links: &[crate::board::BoardLink],
        template_instance: Option<&crate::board::TemplateInstance>,
        fields: Option<&serde_json::Value>,
    ) -> anyhow::Result<crate::board::Board> {
        use crate::board::*;
        let board_id = board_id_for(owner, workspace, key);
        // Fast path: board already persisted. Check the store directly
        // rather than board_info: board_info synthesizes the default
        // board when unpersisted, but board_open must persist it.
        let persisted = self
            .store
            .source_documents()
            .into_iter()
            .find(|d| d.doc_id == board_doc_id(&board_id))
            .filter(|d| {
                d.filters.get(BOARD_ID_FILTER).map(String::as_str) == Some(board_id.as_str())
            });
        if let Some(doc) = persisted {
            if let Some(board) = board_from_doc_content(&doc.content) {
                if let Some(template_instance) = template_instance {
                    if board
                        .template_instance
                        .as_ref()
                        .map(|existing| &existing.content_hash)
                        != Some(&template_instance.content_hash)
                    {
                        anyhow::bail!(
                            "board key already exists with a different template; choose a new key"
                        );
                    }
                    if fields.is_some_and(|fields| board.fields.as_ref() != Some(fields)) {
                        anyhow::bail!("board key already exists with different initial fields");
                    }
                }
                return Ok(board);
            }
        }
        // A new templated board must satisfy its schema's required fields;
        // only a lookup of an existing board may omit them.
        if let Some(instance) = template_instance {
            let empty = serde_json::Value::Object(Default::default());
            validate_template_fields(&instance.snapshot.schema, fields.unwrap_or(&empty))?;
        }
        let board = Board {
            board_id: board_id.clone(),
            owner: owner.to_string(),
            workspace: workspace.to_string(),
            key: key.to_string(),
            title: title.to_string(),
            board_type: board_type.to_string(),
            links: links.to_vec(),
            template_instance: template_instance.cloned(),
            fields: fields.cloned(),
            created_at: Utc::now().to_rfc3339(),
        };
        let doc = crate::SourceDocument {
            doc_id: board_doc_id(&board_id),
            source: "board-registry".to_string(),
            content: board_doc_content(&board).into(),
            concept: format!("board {}", board.key),
            group_id: None,
            headings: vec![],
            links: vec![],
            timestamp: Some(board.created_at.clone()),
            doc_length: board.title.len(),
            author_agent: None,
            filters: board_doc_filters(&board),
            key_phrases: vec![],
            key_phrase_extraction_hash: String::new(),
        };
        // Bypass the enrichment funnel: board definitions are metadata, not
        // searchable content.
        self.store.upsert(doc);
        self.flush()?;
        Ok(board)
    }

    /// Idempotently open the current session's board — the board the
    /// `"default"` alias points to. The board key is the reserved
    /// `session:{session_id}`; the persistent record is created on first
    /// call.
    pub fn board_open_session(
        &mut self,
        owner: &str,
        workspace: &str,
        session_id: &str,
        title: &str,
    ) -> anyhow::Result<crate::board::Board> {
        use crate::board::*;
        let sid = session_id.trim();
        if sid.is_empty() {
            anyhow::bail!("session_id must not be empty");
        }
        // The session key namespace is reserved, so this cannot collide
        // with a user-keyed board.
        self.board_open_inner(
            owner,
            workspace,
            &session_board_key(sid),
            title,
            crate::board::DEFAULT_BOARD_TYPE,
            &[],
            None,
            None,
        )
    }

    /// List boards owned by `owner` in `workspace`. When `session_id`
    /// resolves, the list also includes the default board: the `"default"`
    /// alias *pointing to* the current session's board (synthesized until
    /// its persistent record is created on first post).
    pub fn board_list(
        &self,
        owner: &str,
        workspace: &str,
        session_id: Option<&str>,
    ) -> anyhow::Result<Vec<crate::board::Board>> {
        use crate::board::*;
        let mut boards = Vec::new();
        let sid = session_id.map(str::trim).filter(|s| !s.is_empty());
        for doc in self.store.source_documents() {
            let f = &doc.filters;
            if f.get(BOARD_DOC_KIND_FILTER).map(String::as_str) != Some(BOARD_DOC_KIND) {
                continue;
            }
            if f.get(BOARD_OWNER_FILTER).map(String::as_str) != Some(owner) {
                continue;
            }
            if f.get(BOARD_WORKSPACE_FILTER).map(String::as_str) != Some(workspace) {
                continue;
            }
            if let Some(board) = board_from_doc_content(&doc.content) {
                boards.push(board);
            }
        }
        // The default alias always points at the session's board, whether
        // or not that board's record exists yet. The alias key can never
        // collide with a persisted board (session boards use the
        // "session:" key namespace).
        if let Some(s) = sid {
            boards.push(default_board(owner, workspace, s));
        }
        boards.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(boards)
    }

    /// List persisted boards for a dashboard without synthesizing the
    /// session-specific default alias.
    pub fn board_dashboard_list(
        &self,
        owner: &str,
        workspace: &str,
    ) -> anyhow::Result<Vec<crate::board::Board>> {
        self.board_list(owner, workspace, None)
    }

    /// Read one board and a stable page of its posts for the dashboard.
    pub fn board_dashboard_detail(
        &self,
        board_id: &str,
        owner: &str,
        workspace: &str,
        after_sequence: Option<u64>,
        limit: usize,
    ) -> anyhow::Result<Option<crate::board::BoardSnapshot>> {
        self.board_dashboard_detail_ordered(
            board_id,
            owner,
            workspace,
            after_sequence,
            limit,
            false,
        )
    }

    /// Read the newest page of posts without paging through earlier history.
    pub fn board_dashboard_latest_detail(
        &self,
        board_id: &str,
        owner: &str,
        workspace: &str,
        limit: usize,
    ) -> anyhow::Result<Option<crate::board::BoardSnapshot>> {
        self.board_dashboard_detail_ordered(board_id, owner, workspace, None, limit, true)
    }

    fn board_dashboard_detail_ordered(
        &self,
        board_id: &str,
        owner: &str,
        workspace: &str,
        after_sequence: Option<u64>,
        limit: usize,
        latest: bool,
    ) -> anyhow::Result<Option<crate::board::BoardSnapshot>> {
        use crate::board::*;
        let Some(board) = self.board_info(board_id, owner, workspace, None)? else {
            return Ok(None);
        };
        let after = after_sequence.unwrap_or(0);
        let limit = limit.clamp(1, 100);
        let mut posts = Vec::new();
        let mut latest_sequence = None;
        for doc in self.store.source_documents() {
            let filters = &doc.filters;
            if filters.get(BOARD_DOC_KIND_FILTER).map(String::as_str) != Some(BOARD_POST_KIND)
                || filters.get(BOARD_ID_FILTER).map(String::as_str) != Some(board_id)
            {
                continue;
            }
            let sequence = filters
                .get(BOARD_SEQUENCE_FILTER)
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or_default();
            latest_sequence =
                Some(latest_sequence.map_or(sequence, |latest: u64| latest.max(sequence)));
            if sequence > after {
                posts.push(BoardPost {
                    post_id: doc.doc_id.clone(),
                    board_id: board_id.to_string(),
                    author_agent_id: filters
                        .get(BOARD_AUTHOR_FILTER)
                        .cloned()
                        .unwrap_or_default(),
                    provider: filters
                        .get(BOARD_PROVIDER_FILTER)
                        .cloned()
                        .unwrap_or_default(),
                    title: filters.get(BOARD_POST_TITLE_FILTER).cloned(),
                    session_key: filters.get(BOARD_POST_SESSION_KEY_FILTER).cloned(),
                    content: doc.content.to_string(),
                    links: filters
                        .get(BOARD_POST_LINKS_FILTER)
                        .and_then(|value| serde_json::from_str(value).ok())
                        .unwrap_or_default(),
                    post_template_id: filters.get(BOARD_POST_TEMPLATE_ID_FILTER).cloned(),
                    post_template_version: filters
                        .get(BOARD_POST_TEMPLATE_VERSION_FILTER)
                        .and_then(|v| v.parse().ok()),
                    fields: filters
                        .get(BOARD_POST_FIELDS_FILTER)
                        .and_then(|value| serde_json::from_str(value).ok()),
                    sequence,
                    created_at: doc.timestamp.clone().unwrap_or_default(),
                });
            }
        }
        posts.sort_by_key(|post| post.sequence);
        let has_more = posts.len() > limit;
        if latest && has_more {
            posts.drain(..posts.len() - limit);
        } else {
            posts.truncate(limit);
        }
        Ok(Some(BoardSnapshot {
            board,
            posts,
            has_more,
            latest_sequence,
        }))
    }

    /// Fetch a board by ID. Returns `None` for unknown IDs. The
    /// `"default"` alias resolves to the current session's board
    /// (synthesized until its persistent record is created).
    pub fn board_info(
        &self,
        board_id: &str,
        owner: &str,
        workspace: &str,
        session_id: Option<&str>,
    ) -> anyhow::Result<Option<crate::board::Board>> {
        use crate::board::*;
        // Resolve the default alias to the session board ID first.
        let resolved;
        let board_id = if is_default_alias(board_id) {
            let sid = session_id
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    anyhow::anyhow!("no session_id: cannot resolve the default board alias")
                })?;
            resolved = default_board_id(owner, workspace, sid);
            resolved.as_str()
        } else {
            board_id
        };
        let doc_id = board_doc_id(board_id);
        let doc = self
            .store
            .source_documents()
            .into_iter()
            .find(|d| d.doc_id == doc_id);
        match doc {
            Some(d) => {
                // Verify the board_id filter matches: a forged doc_id must
                // not alias another board.
                if d.filters.get(BOARD_ID_FILTER).map(String::as_str) != Some(board_id) {
                    return Ok(None);
                }
                if d.filters.get(BOARD_OWNER_FILTER).map(String::as_str) != Some(owner)
                    || d.filters.get(BOARD_WORKSPACE_FILTER).map(String::as_str) != Some(workspace)
                {
                    return Ok(None);
                }
                let Some(board) = board_from_doc_content(&d.content) else {
                    return Ok(None);
                };
                if board.owner != owner
                    || board.workspace != workspace
                    || board.board_id != board_id
                {
                    return Ok(None);
                }
                Ok(Some(board))
            }
            None => {
                let sid = session_id.map(str::trim).filter(|s| !s.is_empty());
                match sid {
                    Some(s) if is_default_board(board_id, owner, workspace, s) => {
                        Ok(Some(default_board(owner, workspace, s)))
                    }
                    _ => Ok(None),
                }
            }
        }
    }

    /// Post to a board. `board_id` may be omitted (or be the `"default"`
    /// alias) for the current session's board, whose persistent record is
    /// created on first post. An explicit unknown `board_id` is an error,
    /// never an implicit creation.
    ///
    /// Assigns the next per-board sequence while holding the board lock.
    /// `request_id` makes retries safe: a repeated `request_id` for the same
    /// board returns the original post instead of creating a duplicate —
    /// including across restarts, via a store scan when the in-memory map
    /// misses.
    pub fn board_post(
        &mut self,
        board_id: Option<&str>,
        owner: &str,
        workspace: &str,
        session_id: Option<&str>,
        author_agent_id: &str,
        provider: &str,
        content: &str,
        request_id: &str,
    ) -> anyhow::Result<crate::board::BoardPost> {
        self.board_post_with_metadata(
            board_id,
            owner,
            workspace,
            session_id,
            author_agent_id,
            provider,
            None,
            content,
            &[],
            request_id,
        )
    }

    pub fn board_post_with_metadata(
        &mut self,
        board_id: Option<&str>,
        owner: &str,
        workspace: &str,
        session_id: Option<&str>,
        author_agent_id: &str,
        provider: &str,
        title: Option<&str>,
        content: &str,
        links: &[crate::board::BoardLink],
        request_id: &str,
    ) -> anyhow::Result<crate::board::BoardPost> {
        self.board_post_structured(
            board_id,
            owner,
            workspace,
            session_id,
            author_agent_id,
            provider,
            title,
            content,
            links,
            None,
            None,
            None,
            request_id,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn board_post_structured(
        &mut self,
        board_id: Option<&str>,
        owner: &str,
        workspace: &str,
        session_id: Option<&str>,
        author_agent_id: &str,
        provider: &str,
        title: Option<&str>,
        content: &str,
        links: &[crate::board::BoardLink],
        post_template_id: Option<&str>,
        post_template_version: Option<u32>,
        fields: Option<&serde_json::Value>,
        request_id: &str,
    ) -> anyhow::Result<crate::board::BoardPost> {
        use crate::board::*;
        if request_id.trim().is_empty() || request_id.len() > 256 {
            anyhow::bail!("request_id must be 1-256 bytes");
        }
        if let Some(title) = title {
            if title.trim().is_empty() || title.len() > 256 {
                anyhow::bail!("post title must be 1-256 characters when provided");
            }
        }
        let links = normalize_board_links(links)?;
        self.validate_board_links(&links, owner, workspace)?;
        let board_id = self.resolve_board_id(board_id, owner, workspace, session_id, true)?;
        // The board must exist; posting never creates one implicitly
        // (the session board is ensured by resolve_board_id above).
        if self
            .board_info(&board_id, owner, workspace, session_id)?
            .is_none()
        {
            anyhow::bail!("unknown board_id: {board_id}");
        }
        let board = self
            .board_info(&board_id, owner, workspace, session_id)?
            .ok_or_else(|| anyhow::anyhow!("unknown board_id: {board_id}"))?;
        match (&board.template_instance, post_template_id, post_template_version, fields) {
            (Some(instance), Some(template_id), Some(version), Some(fields)) => {
                let post_template = instance.snapshot.post_templates.iter()
                    .find(|item| item.template_id == template_id && item.version == version)
                    .ok_or_else(|| anyhow::anyhow!("post template {template_id:?}@{version} is not in the board's pinned template snapshot"))?;
                validate_template_fields(&post_template.schema, fields)?;
                if serde_json::to_vec(fields)?.len() > TEMPLATE_FIELDS_MAX_BYTES {
                    anyhow::bail!("structured post fields must be at most 32768 bytes");
                }
            }
            (Some(_), _, _, _) => anyhow::bail!("templated boards require post_template_id, post_template_version, and fields from the pinned snapshot on every post"),
            (None, None, None, None) => {}
            (None, _, _, _) => anyhow::bail!("post template metadata and fields require a board with template_instance"),
        }
        // A retry returns the original post only if the complete normalized
        // request matches. Reusing an ID for different content is a conflict,
        // both in-process and after restart.
        let expected_title = title.map(str::trim).map(str::to_string);
        let expected_session_key = session_id
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(crate::telemetry::opaque_session_key);
        let validate_retry =
            |post: crate::board::BoardPost| -> anyhow::Result<crate::board::BoardPost> {
                anyhow::ensure!(
                    post.author_agent_id == author_agent_id
                        && post.provider == provider
                        && post.title == expected_title
                        && post.session_key == expected_session_key
                        && post.content == content
                        && post.links == links
                        && post.post_template_id.as_deref() == post_template_id
                        && post.post_template_version == post_template_version
                        && post.fields.as_ref() == fields,
                    "request_id was reused with different board post content"
                );
                Ok(post)
            };
        let cached = {
            let state = self.board_state.lock().expect("board state lock poisoned");
            state
                .request_ids
                .get(&(board_id.clone(), request_id.to_string()))
                .cloned()
        };
        if let Some(post_id) = cached {
            let post = self
                .board_get(Some(&board_id), owner, workspace, session_id, &post_id)?
                .ok_or_else(|| anyhow::anyhow!("request_id mapped to missing post"))?;
            return validate_retry(post);
        }
        if let Some(post_id) = self.board_find_request_id(&board_id, request_id)? {
            // Repopulate the map so later retries stay cheap.
            let mut state = self.board_state.lock().expect("board state lock poisoned");
            state
                .request_ids
                .insert((board_id.clone(), request_id.to_string()), post_id.clone());
            drop(state);
            let post = self
                .board_get(Some(&board_id), owner, workspace, session_id, &post_id)?
                .ok_or_else(|| anyhow::anyhow!("request_id mapped to missing post"))?;
            return validate_retry(post);
        }
        // Assign the next sequence. Lazily initialize from the store's max
        // so a restart recovers without gaps or reuse.
        // Assign the next sequence. Lazily initialize from the store's max
        // so a restart recovers without gaps or reuse.
        let mut state = self.board_state.lock().expect("board state lock poisoned");
        let sequence = match state.next_sequence.get(board_id.as_str()) {
            Some(&next) => next,
            None => {
                let max = self.board_max_sequence(&board_id)?;
                let next = max + 1;
                state.next_sequence.insert(board_id.clone(), next + 1);
                next
            }
        };
        // Bump the counter for the next post.
        state.next_sequence.insert(board_id.clone(), sequence + 1);
        let post = BoardPost {
            post_id: board_post_doc_id(&board_id, sequence),
            board_id: board_id.clone(),
            author_agent_id: author_agent_id.to_string(),
            provider: provider.to_string(),
            title: title.map(|value| value.trim().to_string()),
            session_key: session_id
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(crate::telemetry::opaque_session_key),
            content: content.to_string(),
            links,
            post_template_id: post_template_id.map(str::to_string),
            post_template_version,
            fields: fields.cloned(),
            sequence,
            created_at: Utc::now().to_rfc3339(),
        };
        state.request_ids.insert(
            (board_id.clone(), request_id.to_string()),
            post.post_id.clone(),
        );
        drop(state);
        let doc = crate::SourceDocument {
            doc_id: post.post_id.clone(),
            source: "board-post".to_string(),
            content: post.content.as_str().into(),
            concept: format!("board post {}:{}", board_id, sequence),
            group_id: Some(board_id.clone()),
            headings: vec![],
            links: vec![],
            timestamp: Some(post.created_at.clone()),
            doc_length: post.content.len(),
            author_agent: Some(author_agent_id.to_string()),
            filters: board_post_doc_filters(&post, request_id),
            key_phrases: vec![],
            key_phrase_extraction_hash: String::new(),
        };
        self.upsert(doc);
        self.flush()?;
        Ok(post)
    }

    /// Find a post by (board_id, request_id) in the store. Used for
    /// retry idempotency across restarts, when the in-memory map is cold.
    fn board_find_request_id(
        &self,
        board_id: &str,
        request_id: &str,
    ) -> anyhow::Result<Option<String>> {
        use crate::board::*;
        for doc in self.store.source_documents() {
            let f = &doc.filters;
            if f.get(BOARD_DOC_KIND_FILTER).map(String::as_str) != Some(BOARD_POST_KIND) {
                continue;
            }
            if f.get(BOARD_ID_FILTER).map(String::as_str) != Some(board_id) {
                continue;
            }
            if f.get(BOARD_REQUEST_ID_FILTER).map(String::as_str) == Some(request_id) {
                return Ok(Some(doc.doc_id.clone()));
            }
        }
        Ok(None)
    }

    /// Max assigned sequence for a board, from the store. Used to
    /// initialize the in-memory counter after a restart.
    fn board_max_sequence(&self, board_id: &str) -> anyhow::Result<u64> {
        use crate::board::*;
        let mut max = 0u64;
        for doc in self.store.source_documents() {
            let f = &doc.filters;
            if f.get(BOARD_DOC_KIND_FILTER).map(String::as_str) != Some(BOARD_POST_KIND) {
                continue;
            }
            if f.get(BOARD_ID_FILTER).map(String::as_str) != Some(board_id) {
                continue;
            }
            if let Some(seq_str) = f.get(BOARD_SEQUENCE_FILTER) {
                if let Ok(seq) = seq_str.parse::<u64>() {
                    max = max.max(seq);
                }
            }
        }
        Ok(max)
    }

    /// Read posts in posting order. `board_id` may be omitted (or be the
    /// `"default"` alias) for the current session's board.
    /// `after_sequence` is exclusive; pass 0 (or None) to read from the
    /// start. A never-posted-to board reads as empty, not an error.
    pub fn board_read(
        &mut self,
        board_id: Option<&str>,
        owner: &str,
        workspace: &str,
        session_id: Option<&str>,
        after_sequence: Option<u64>,
        limit: usize,
    ) -> anyhow::Result<Vec<crate::board::BoardPost>> {
        use crate::board::*;
        let board_id = self.resolve_board_id(board_id, owner, workspace, session_id, false)?;
        let after = after_sequence.unwrap_or(0);
        let limit = limit.clamp(1, 100);
        let mut posts = Vec::new();
        for doc in self.store.source_documents() {
            let f = &doc.filters;
            if f.get(BOARD_DOC_KIND_FILTER).map(String::as_str) != Some(BOARD_POST_KIND) {
                continue;
            }
            if f.get(BOARD_ID_FILTER).map(String::as_str) != Some(board_id.as_str()) {
                continue;
            }
            let sequence: u64 = f
                .get(BOARD_SEQUENCE_FILTER)
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            if sequence <= after {
                continue;
            }
            posts.push(BoardPost {
                post_id: doc.doc_id.clone(),
                board_id: board_id.clone(),
                author_agent_id: f.get(BOARD_AUTHOR_FILTER).cloned().unwrap_or_default(),
                provider: f.get(BOARD_PROVIDER_FILTER).cloned().unwrap_or_default(),
                title: f.get(BOARD_POST_TITLE_FILTER).cloned(),
                session_key: f.get(BOARD_POST_SESSION_KEY_FILTER).cloned(),
                content: doc.content.to_string(),
                links: f
                    .get(BOARD_POST_LINKS_FILTER)
                    .and_then(|value| serde_json::from_str(value).ok())
                    .unwrap_or_default(),
                post_template_id: f.get(BOARD_POST_TEMPLATE_ID_FILTER).cloned(),
                post_template_version: f
                    .get(BOARD_POST_TEMPLATE_VERSION_FILTER)
                    .and_then(|v| v.parse().ok()),
                fields: f
                    .get(BOARD_POST_FIELDS_FILTER)
                    .and_then(|value| serde_json::from_str(value).ok()),
                sequence,
                created_at: doc.timestamp.clone().unwrap_or_default(),
            });
        }
        posts.sort_by_key(|p| p.sequence);
        posts.truncate(limit);
        Ok(posts)
    }

    /// Get one post. `board_id` may be omitted (or be the `"default"`
    /// alias) for the current session's board. Returns `None` when the
    /// post doesn't exist **or** belongs to a different board: a post ID
    /// from another board cannot be read through the wrong board.
    pub fn board_get(
        &mut self,
        board_id: Option<&str>,
        owner: &str,
        workspace: &str,
        session_id: Option<&str>,
        post_id: &str,
    ) -> anyhow::Result<Option<crate::board::BoardPost>> {
        use crate::board::*;
        let board_id = self.resolve_board_id(board_id, owner, workspace, session_id, false)?;
        let doc = self
            .store
            .source_documents()
            .into_iter()
            .find(|d| d.doc_id == post_id);
        match doc {
            Some(d) => {
                let f = &d.filters;
                if f.get(BOARD_DOC_KIND_FILTER).map(String::as_str) != Some(BOARD_POST_KIND) {
                    return Ok(None);
                }
                // Cross-board isolation: the post's board must match.
                if f.get(BOARD_ID_FILTER).map(String::as_str) != Some(board_id.as_str()) {
                    return Ok(None);
                }
                let sequence: u64 = f
                    .get(BOARD_SEQUENCE_FILTER)
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                Ok(Some(BoardPost {
                    post_id: d.doc_id.clone(),
                    board_id: board_id.clone(),
                    author_agent_id: f.get(BOARD_AUTHOR_FILTER).cloned().unwrap_or_default(),
                    provider: f.get(BOARD_PROVIDER_FILTER).cloned().unwrap_or_default(),
                    title: f.get(BOARD_POST_TITLE_FILTER).cloned(),
                    session_key: f.get(BOARD_POST_SESSION_KEY_FILTER).cloned(),
                    content: d.content.to_string(),
                    links: f
                        .get(BOARD_POST_LINKS_FILTER)
                        .and_then(|value| serde_json::from_str(value).ok())
                        .unwrap_or_default(),
                    post_template_id: f.get(BOARD_POST_TEMPLATE_ID_FILTER).cloned(),
                    post_template_version: f
                        .get(BOARD_POST_TEMPLATE_VERSION_FILTER)
                        .and_then(|v| v.parse().ok()),
                    fields: f
                        .get(BOARD_POST_FIELDS_FILTER)
                        .and_then(|value| serde_json::from_str(value).ok()),
                    sequence,
                    created_at: d.timestamp.clone().unwrap_or_default(),
                }))
            }
            None => Ok(None),
        }
    }

    /// Search a board's posts. `board_id` may be omitted (or be the
    /// `"default"` alias) for the current session's board. Scoped to the
    /// resolved board: results never leak posts from other boards. A
    /// never-posted-to board searches as empty, not an error.
    pub fn board_search(
        &mut self,
        board_id: Option<&str>,
        owner: &str,
        workspace: &str,
        session_id: Option<&str>,
        query: &str,
        top_k: usize,
    ) -> anyhow::Result<Vec<crate::board::BoardPost>> {
        use crate::board::*;
        let board_id = self.resolve_board_id(board_id, owner, workspace, session_id, false)?;
        let mut filters = BTreeMap::new();
        filters.insert(
            BOARD_DOC_KIND_FILTER.to_string(),
            BOARD_POST_KIND.to_string(),
        );
        filters.insert(BOARD_ID_FILTER.to_string(), board_id.clone());
        // Scope to the board's posts; no user filter (board posts are shared
        // within the board's owner/workspace scope).
        let results = self.search_with_filters(
            query,
            /* scope = */ board_id.as_str(),
            /* session_id = */ None,
            top_k.clamp(1, 50),
            &filters,
            None,
        )?;
        let mut posts = Vec::new();
        for r in results {
            // search_with_filters already applied the board_id filter, but
            // verify again: never leak across boards.
            let doc = self.store.source_document_by_id(&r.doc_id).filter(|d| {
                d.filters.get(BOARD_ID_FILTER).map(String::as_str) == Some(board_id.as_str())
            });
            let Some(d) = doc else { continue };
            let f = &d.filters;
            let sequence: u64 = f
                .get(BOARD_SEQUENCE_FILTER)
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            posts.push(BoardPost {
                post_id: d.doc_id.clone(),
                board_id: board_id.clone(),
                author_agent_id: f.get(BOARD_AUTHOR_FILTER).cloned().unwrap_or_default(),
                provider: f.get(BOARD_PROVIDER_FILTER).cloned().unwrap_or_default(),
                title: f.get(BOARD_POST_TITLE_FILTER).cloned(),
                session_key: f.get(BOARD_POST_SESSION_KEY_FILTER).cloned(),
                content: d.content.to_string(),
                links: f
                    .get(BOARD_POST_LINKS_FILTER)
                    .and_then(|value| serde_json::from_str(value).ok())
                    .unwrap_or_default(),
                post_template_id: f.get(BOARD_POST_TEMPLATE_ID_FILTER).cloned(),
                post_template_version: f
                    .get(BOARD_POST_TEMPLATE_VERSION_FILTER)
                    .and_then(|v| v.parse().ok()),
                fields: f
                    .get(BOARD_POST_FIELDS_FILTER)
                    .and_then(|value| serde_json::from_str(value).ok()),
                sequence,
                created_at: d.timestamp.clone().unwrap_or_default(),
            });
        }
        Ok(posts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::board::*;
    use crate::PipelineOptions;
    use std::sync::{Arc, Mutex};

    const OWNER: &str = "mcp";
    const WS: &str = "ws-board-test";
    const S1: &str = "sess-1";
    const S2: &str = "sess-2";

    fn svc() -> MemoryService {
        MemoryService::in_memory(PipelineOptions::default())
    }

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let base = std::env::temp_dir()
            .canonicalize()
            .unwrap_or_else(|_| std::env::temp_dir());
        let dir = base.join(format!("membrick-board-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn post(
        service: &mut MemoryService,
        board_id: Option<&str>,
        session: Option<&str>,
        content: &str,
        request_id: &str,
    ) -> BoardPost {
        service
            .board_post(
                board_id, OWNER, WS, session, "agent-1", "claude", content, request_id,
            )
            .unwrap()
    }

    #[test]
    fn default_board_listed_before_persistence() {
        let service = svc();
        let boards = service.board_list(OWNER, WS, Some(S1)).unwrap();
        let alias = boards.iter().find(|b| b.key == DEFAULT_BOARD_KEY).unwrap();
        assert_eq!(alias.board_id, default_board_id(OWNER, WS, S1));
        // board_info synthesizes the default too.
        let info = service
            .board_info("default", OWNER, WS, Some(S1))
            .unwrap()
            .unwrap();
        assert_eq!(info.board_id, default_board_id(OWNER, WS, S1));
    }

    #[test]
    fn no_session_no_default_entry() {
        let service = svc();
        let boards = service.board_list(OWNER, WS, None).unwrap();
        assert!(boards.iter().all(|b| b.key != DEFAULT_BOARD_KEY));
        assert!(service.board_info("default", OWNER, WS, None).is_err());
    }

    #[test]
    fn first_default_post_creates_persistent_record() {
        let mut service = svc();
        let p = post(&mut service, None, Some(S1), "hello", "r1");
        assert_eq!(p.sequence, 1);
        assert_eq!(p.board_id, default_board_id(OWNER, WS, S1));
        // The record is now persistent: board_info finds it by real ID.
        let info = service
            .board_info(&p.board_id, OWNER, WS, Some(S1))
            .unwrap()
            .unwrap();
        assert!(!info.created_at.is_empty());
        assert_eq!(info.key, session_board_key(S1));
    }

    #[test]
    fn omitted_and_alias_resolve_to_same_board() {
        let mut service = svc();
        let a = post(&mut service, None, Some(S1), "a", "r1");
        let b = post(&mut service, Some("default"), Some(S1), "b", "r2");
        assert_eq!(a.board_id, b.board_id);
        assert_eq!(b.sequence, 2);
    }

    #[test]
    fn default_requires_session() {
        let mut service = svc();
        let err = service
            .board_post(None, OWNER, WS, None, "agent-1", "claude", "x", "r1")
            .unwrap_err();
        assert!(err.to_string().contains("session_id"), "{err}");
    }

    #[test]
    fn empty_default_read_and_search() {
        let mut service = svc();
        let posts = service
            .board_read(None, OWNER, WS, Some(S1), None, 20)
            .unwrap();
        assert!(posts.is_empty());
        let hits = service
            .board_search(None, OWNER, WS, Some(S1), "anything", 10)
            .unwrap();
        assert!(hits.is_empty());
        assert!(service
            .board_get(None, OWNER, WS, Some(S1), "nope")
            .unwrap()
            .is_none());
    }

    #[test]
    fn explicit_unknown_board_post_errors() {
        let mut service = svc();
        let err = post_unknown(&mut service).unwrap_err();
        assert!(err.to_string().contains("unknown board_id"), "{err}");
    }

    fn post_unknown(service: &mut MemoryService) -> anyhow::Result<BoardPost> {
        service.board_post(
            Some("board:nope:nope:nope"),
            OWNER,
            WS,
            Some(S1),
            "agent-1",
            "claude",
            "x",
            "r1",
        )
    }

    #[test]
    fn board_open_is_idempotent() {
        let mut service = svc();
        let a = service.board_open(OWNER, WS, "task-a", "Task A").unwrap();
        let b = service
            .board_open(OWNER, WS, "task-a", "Different title")
            .unwrap();
        assert_eq!(a.board_id, b.board_id);
        assert_eq!(a.title, "Task A");
    }

    #[test]
    fn reserved_keys_rejected() {
        let mut service = svc();
        assert!(service.board_open(OWNER, WS, "session:evil", "x").is_err());
        assert!(service.board_open(OWNER, WS, "default", "x").is_err());
        // The MCP-facing session opener still works.
        let b = service.board_open_session(OWNER, WS, S1, "T").unwrap();
        assert_eq!(b.board_id, default_board_id(OWNER, WS, S1));
    }

    #[test]
    fn sessions_are_isolated() {
        let mut service = svc();
        let p1 = post(&mut service, None, Some(S1), "hello s1", "r1");
        let p2 = post(&mut service, None, Some(S2), "hello s2", "r1");
        assert_ne!(p1.board_id, p2.board_id);
        // Same request_id on different sessions' boards: no collision.
        assert_ne!(p1.post_id, p2.post_id);
        let s1_posts = service
            .board_read(None, OWNER, WS, Some(S1), None, 20)
            .unwrap();
        assert_eq!(s1_posts.len(), 1);
        assert_eq!(s1_posts[0].sequence, 1);
        assert_eq!(s1_posts[0].content, "hello s1");
        let s2_posts = service
            .board_read(None, OWNER, WS, Some(S2), None, 20)
            .unwrap();
        assert_eq!(s2_posts.len(), 1);
        assert_eq!(s2_posts[0].content, "hello s2");
    }

    #[test]
    fn sequences_increase_per_board() {
        let mut service = svc();
        let named = service.board_open(OWNER, WS, "k", "K").unwrap();
        for i in 1..=3u64 {
            let p = post(
                &mut service,
                Some(&named.board_id),
                Some(S1),
                &format!("n{i}"),
                &format!("nr{i}"),
            );
            assert_eq!(p.sequence, i);
        }
        // The default board has its own sequence space.
        let d = post(&mut service, None, Some(S1), "d1", "dr1");
        assert_eq!(d.sequence, 1);
    }

    #[test]
    fn request_id_retry_returns_original() {
        let mut service = svc();
        let a = post(&mut service, None, Some(S1), "first", "retry-1");
        let b = post(&mut service, None, Some(S1), "second", "retry-1");
        assert_eq!(a.post_id, b.post_id);
        assert_eq!(b.content, "first");
        assert_eq!(b.sequence, 1);
        let posts = service
            .board_read(None, OWNER, WS, Some(S1), None, 20)
            .unwrap();
        assert_eq!(posts.len(), 1);
    }

    #[test]
    fn after_sequence_catchup() {
        let mut service = svc();
        post(&mut service, None, Some(S1), "one", "r1");
        post(&mut service, None, Some(S1), "two", "r2");
        let posts = service
            .board_read(None, OWNER, WS, Some(S1), Some(1), 20)
            .unwrap();
        assert_eq!(posts.len(), 1);
        assert_eq!(posts[0].content, "two");
    }

    #[test]
    fn wrong_board_post_id_rejected() {
        let mut service = svc();
        let named = service.board_open(OWNER, WS, "k", "K").unwrap();
        let p = post(
            &mut service,
            Some(&named.board_id),
            Some(S1),
            "secret",
            "r1",
        );
        // Same post ID through the default board: not found.
        assert!(service
            .board_get(None, OWNER, WS, Some(S1), &p.post_id)
            .unwrap()
            .is_none());
        // Through the right board: found.
        assert!(service
            .board_get(Some(&named.board_id), OWNER, WS, Some(S1), &p.post_id)
            .unwrap()
            .is_some());
    }

    #[test]
    fn focused_and_default_boards_isolated() {
        let mut service = svc();
        let named = service.board_open(OWNER, WS, "focus", "Focus").unwrap();
        post(
            &mut service,
            Some(&named.board_id),
            Some(S1),
            "focused note",
            "r1",
        );
        // Default board is untouched.
        assert!(service
            .board_read(None, OWNER, WS, Some(S1), None, 20)
            .unwrap()
            .is_empty());
        // Search on the default board doesn't leak the focused post.
        assert!(service
            .board_search(None, OWNER, WS, Some(S1), "focused", 10)
            .unwrap()
            .is_empty());
        // And vice versa.
        post(&mut service, None, Some(S1), "default note", "r2");
        assert!(service
            .board_search(Some(&named.board_id), OWNER, WS, Some(S1), "default", 10)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn board_search_finds_posts() {
        let mut service = svc();
        post(
            &mut service,
            None,
            Some(S1),
            "the parser failure comes from empty input",
            "r1",
        );
        post(
            &mut service,
            None,
            Some(S1),
            "unrelated status update",
            "r2",
        );
        let hits = service
            .board_search(None, OWNER, WS, Some(S1), "parser failure", 10)
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].content.contains("parser failure"));
    }

    #[test]
    fn owner_workspace_scoping() {
        let mut service = svc();
        post(&mut service, None, Some(S1), "x", "r1");
        // Other workspace: no boards, default alias resolves to its own board.
        let other = service.board_list(OWNER, "other-ws", Some(S1)).unwrap();
        assert_eq!(other.len(), 1);
        assert_eq!(other[0].key, DEFAULT_BOARD_KEY);
        assert_eq!(other[0].board_id, default_board_id(OWNER, "other-ws", S1));
        // Other owner: same.
        let other_owner = service.board_list("someone-else", WS, Some(S1)).unwrap();
        assert_eq!(other_owner.len(), 1);
        assert_eq!(
            other_owner[0].board_id,
            default_board_id("someone-else", WS, S1)
        );
    }

    #[test]
    fn request_id_and_sequence_survive_restart() {
        let dir = temp_dir("restart");
        let options = PipelineOptions::default();
        let mut service = MemoryService::at_path(&dir, options.clone()).unwrap();
        let p1 = post(&mut service, None, Some(S1), "first", "keep-me");
        assert_eq!(p1.sequence, 1);
        post(&mut service, None, Some(S1), "second", "r2");
        drop(service);
        // Reopen: the store scan recovers request IDs and sequences.
        let mut reopened = MemoryService::at_path(&dir, options).unwrap();
        let retry = post(&mut reopened, None, Some(S1), "changed", "keep-me");
        assert_eq!(retry.post_id, p1.post_id);
        assert_eq!(retry.content, "first");
        let p3 = post(&mut reopened, None, Some(S1), "third", "r3");
        assert_eq!(p3.sequence, 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn concurrent_posts_get_unique_sequences() {
        let service = Arc::new(Mutex::new(svc()));
        let mut handles = Vec::new();
        for t in 0..8u32 {
            let service = Arc::clone(&service);
            handles.push(std::thread::spawn(move || {
                for i in 0..5u32 {
                    let mut guard = service.lock().unwrap();
                    post(
                        &mut guard,
                        None,
                        Some(S1),
                        &format!("t{t} i{i}"),
                        &format!("conc-{t}-{i}"),
                    );
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let mut guard = service.lock().unwrap();
        let posts = guard
            .board_read(None, OWNER, WS, Some(S1), None, 100)
            .unwrap();
        assert_eq!(posts.len(), 40);
        let mut seqs: Vec<u64> = posts.iter().map(|p| p.sequence).collect();
        seqs.sort_unstable();
        assert_eq!(seqs, (1..=40).collect::<Vec<_>>());
    }

    use crate::board::test_fixtures::*;
    use serde_json::{json, Value};

    fn open_project(
        service: &mut MemoryService,
        key: &str,
        instance: &TemplateInstance,
        fields: Option<&Value>,
    ) -> anyhow::Result<Board> {
        service.board_open_instance(
            OWNER,
            WS,
            key,
            "Project",
            "project",
            &[],
            Some(instance),
            fields,
        )
    }

    fn post_task(
        service: &mut MemoryService,
        board_id: &str,
        template: Option<(&str, u32)>,
        fields: Option<&Value>,
        request_id: &str,
    ) -> anyhow::Result<BoardPost> {
        service.board_post_structured(
            Some(board_id),
            OWNER,
            WS,
            Some(S1),
            "agent-1",
            "claude",
            Some("Task"),
            "narrative",
            &[],
            template.map(|(id, _)| id),
            template.map(|(_, version)| version),
            fields,
            request_id,
        )
    }

    fn instance_from(value: Value) -> TemplateInstance {
        serde_json::from_value(value).unwrap()
    }

    fn task_fields() -> Value {
        json!({"description": "Write tests", "status": "active", "future_field": [1]})
    }

    #[test]
    fn templated_board_open_pins_snapshot_and_is_idempotent() {
        let mut service = svc();
        let instance = project_instance(1);
        let fields = project_fields();
        let board = open_project(&mut service, "launch", &instance, Some(&fields)).unwrap();
        let pinned = board.template_instance.as_ref().unwrap();
        assert_eq!(pinned.content_hash, instance.content_hash);
        assert_eq!(pinned.snapshot.post_templates.len(), 1);
        assert_eq!(board.fields.as_ref(), Some(&fields));
        // Same key, same hash: the original board, unchanged.
        let again = open_project(&mut service, "launch", &instance, Some(&fields)).unwrap();
        assert_eq!(again.board_id, board.board_id);
        assert_eq!(again.created_at, board.created_at);
        // Same key, same hash, no fields: still the same board.
        let lookup = open_project(&mut service, "launch", &instance, None).unwrap();
        assert_eq!(lookup.fields.as_ref(), Some(&fields));
        // A plain key/title open returns the templated board as-is.
        let plain = service.board_open(OWNER, WS, "launch", "x").unwrap();
        assert!(plain.template_instance.is_some());
    }

    #[test]
    fn reopen_with_conflicting_template_or_fields_fails_clearly() {
        let mut service = svc();
        open_project(
            &mut service,
            "launch",
            &project_instance(1),
            Some(&project_fields()),
        )
        .unwrap();
        let err = open_project(
            &mut service,
            "launch",
            &project_instance(2),
            Some(&project_fields()),
        )
        .unwrap_err();
        assert!(err.to_string().contains("different template"), "{err}");
        let mut other_fields = project_fields();
        other_fields["goal"] = json!("Another goal");
        let err = open_project(
            &mut service,
            "launch",
            &project_instance(1),
            Some(&other_fields),
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("different initial fields"),
            "{err}"
        );
        // An untemplated board cannot be upgraded in place.
        service.board_open(OWNER, WS, "plain", "Plain").unwrap();
        let err = open_project(
            &mut service,
            "plain",
            &project_instance(1),
            Some(&project_fields()),
        )
        .unwrap_err();
        assert!(err.to_string().contains("different template"), "{err}");
    }

    #[test]
    fn tampered_hashes_are_rejected() {
        let mut service = svc();
        let mut wrong_board = project_instance(1);
        wrong_board.content_hash = format!("sha256:{}", "0".repeat(64));
        let err =
            open_project(&mut service, "a", &wrong_board, Some(&project_fields())).unwrap_err();
        assert!(
            err.to_string()
                .contains("board template content_hash mismatch"),
            "{err}"
        );

        let mut wrong_post = project_instance_with(1, Vec::new());
        wrong_post["snapshot"]["post_templates"][0]["content_hash"] =
            json!(format!("sha256:{}", "1".repeat(64)));
        let err = open_project(
            &mut service,
            "b",
            &instance_from(wrong_post),
            Some(&project_fields()),
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("post template content_hash mismatch"),
            "{err}"
        );

        // Schema edited after hashing: the pinned hash no longer matches.
        let mut edited = project_instance_with(1, Vec::new());
        edited["snapshot"]["schema"]["fields"][0]["required"] = json!(false);
        let err = open_project(
            &mut service,
            "c",
            &instance_from(edited),
            Some(&project_fields()),
        )
        .unwrap_err();
        assert!(err.to_string().contains("content_hash mismatch"), "{err}");
        assert!(service.board_list(OWNER, WS, None).unwrap().is_empty());
    }

    #[test]
    fn snapshot_bounds_are_enforced() {
        let mut service = svc();
        let fields = project_fields();
        // Oversized: many large unknown metadata strings inside the snapshot.
        let mut big = project_instance_with(1, Vec::new());
        for i in 0..10 {
            big["snapshot"][format!("x_blob_{i}")] = json!("x".repeat(8000));
        }
        let err =
            open_project(&mut service, "big", &instance_from(big), Some(&fields)).unwrap_err();
        assert!(err.to_string().contains("exceeds 65536 bytes"), "{err}");

        let many: Vec<Value> = (0..32)
            .map(|_| json!({"kind": "fields", "field_ids": ["goal"]}))
            .collect();
        let err = open_project(
            &mut service,
            "many",
            &instance_from(project_instance_with(1, many)),
            Some(&fields),
        )
        .unwrap_err();
        assert!(err.to_string().contains("at most 32 components"), "{err}");

        let mut deep = json!("leaf");
        for _ in 0..20 {
            deep = json!({ "n": deep });
        }
        let mut nested = project_instance_with(1, Vec::new());
        nested["snapshot"]["x_deep"] = deep;
        let err =
            open_project(&mut service, "deep", &instance_from(nested), Some(&fields)).unwrap_err();
        assert!(err.to_string().contains("nesting exceeds 16"), "{err}");

        let bad_ref = project_instance_with(
            1,
            vec![json!({"kind": "fields", "field_ids": ["not_in_schema"]})],
        );
        let err =
            open_project(&mut service, "ref", &instance_from(bad_ref), Some(&fields)).unwrap_err();
        assert!(err.to_string().contains("unknown schema field"), "{err}");

        let mut unknown_type = project_instance_with(1, Vec::new());
        unknown_type["snapshot"]["schema"]["fields"][0]["type"] = json!("date");
        let err = serde_json::from_value::<TemplateInstance>(unknown_type).unwrap_err();
        assert!(err.to_string().contains("unknown variant"), "{err}");
    }

    #[test]
    fn board_fields_are_validated_against_the_pinned_schema() {
        let mut service = svc();
        let instance = project_instance(1);
        let err =
            open_project(&mut service, "a", &instance, Some(&json!({"goal": "x"}))).unwrap_err();
        assert!(err.to_string().contains("success_criteria"), "{err}");
        let err = open_project(
            &mut service,
            "b",
            &instance,
            Some(&json!({"goal": 1, "success_criteria": []})),
        )
        .unwrap_err();
        assert!(err.to_string().contains("wrong type"), "{err}");
        // Creating a templated board without its required fields must fail,
        // not persist a board whose hero has nothing to show.
        let err = open_project(&mut service, "c", &instance, None).unwrap_err();
        assert!(err.to_string().contains("missing required field"), "{err}");
        let err = service
            .board_open_instance(OWNER, WS, "d", "D", "general", &[], None, Some(&json!({})))
            .unwrap_err();
        assert!(
            err.to_string().contains("require a template_instance"),
            "{err}"
        );
    }

    #[test]
    fn templated_posts_bind_to_the_pinned_post_template() {
        let mut service = svc();
        let board = open_project(
            &mut service,
            "launch",
            &project_instance(1),
            Some(&project_fields()),
        )
        .unwrap();
        let id = board.board_id.as_str();
        let fields = task_fields();
        let p = post_task(&mut service, id, Some(("task", 1)), Some(&fields), "t1").unwrap();
        assert_eq!(p.post_template_id.as_deref(), Some("task"));
        assert_eq!(p.post_template_version, Some(1));
        assert_eq!(p.fields.as_ref(), Some(&fields));

        for (template, needle) in [
            (("task", 2), "not in the board's pinned"),
            (("plan", 1), "not in the board's pinned"),
        ] {
            let err = post_task(&mut service, id, Some(template), Some(&fields), "x").unwrap_err();
            assert!(err.to_string().contains(needle), "{err}");
        }
        for (bad, needle) in [
            (json!({"status": "active"}), "missing required field"),
            (
                json!({"description": "d", "status": "paused"}),
                "declared enum",
            ),
            (
                json!({"description": "d", "status": "done", "estimate": "3"}),
                "wrong type",
            ),
        ] {
            let err = post_task(&mut service, id, Some(("task", 1)), Some(&bad), "x").unwrap_err();
            assert!(err.to_string().contains(needle), "{bad}: {err}");
        }
        let err = post_task(&mut service, id, None, None, "free").unwrap_err();
        assert!(
            err.to_string().contains("templated boards require"),
            "{err}"
        );
        // Nothing but the valid post was written.
        let posts = service
            .board_read(Some(id), OWNER, WS, Some(S1), None, 20)
            .unwrap();
        assert_eq!(posts.len(), 1);
    }

    #[test]
    fn untemplated_boards_keep_free_form_posts() {
        let mut service = svc();
        let board = service.board_open(OWNER, WS, "plain", "Plain").unwrap();
        let p = post_task(&mut service, &board.board_id, None, None, "f1").unwrap();
        assert!(p.fields.is_none() && p.post_template_id.is_none());
        let err = post_task(
            &mut service,
            &board.board_id,
            Some(("task", 1)),
            Some(&task_fields()),
            "f2",
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("require a board with template_instance"),
            "{err}"
        );
    }

    #[test]
    fn templated_post_request_id_retry_returns_original() {
        let mut service = svc();
        let board = open_project(
            &mut service,
            "launch",
            &project_instance(1),
            Some(&project_fields()),
        )
        .unwrap();
        let first = post_task(
            &mut service,
            &board.board_id,
            Some(("task", 1)),
            Some(&task_fields()),
            "same",
        )
        .unwrap();
        let mut changed = task_fields();
        changed["status"] = json!("done");
        let retry = post_task(
            &mut service,
            &board.board_id,
            Some(("task", 1)),
            Some(&changed),
            "same",
        )
        .unwrap();
        assert_eq!(retry.post_id, first.post_id);
        assert_eq!(retry.fields, first.fields);
    }

    #[test]
    fn template_data_survives_restart_on_every_read_path() {
        let dir = temp_dir("template-restart");
        let options = PipelineOptions::default();
        let instance = project_instance(1);
        let target = {
            let mut service = MemoryService::at_path(&dir, options.clone()).unwrap();
            service
                .board_open(OWNER, WS, "milestone", "Milestone")
                .unwrap()
        };
        let links = vec![BoardLink {
            target: target.board_id.clone(),
            relation: "milestone".into(),
        }];
        let (board_id, post_id) = {
            let mut service = MemoryService::at_path(&dir, options.clone()).unwrap();
            let board = service
                .board_open_instance(
                    OWNER,
                    WS,
                    "launch",
                    "Launch",
                    "project",
                    &links,
                    Some(&instance),
                    Some(&project_fields()),
                )
                .unwrap();
            let post = service
                .board_post_structured(
                    Some(&board.board_id),
                    OWNER,
                    WS,
                    Some(S1),
                    "agent-1",
                    "claude",
                    Some("Task"),
                    "narrative",
                    &links,
                    Some("task"),
                    Some(1),
                    Some(&task_fields()),
                    "persist-1",
                )
                .unwrap();
            (board.board_id, post.post_id)
        };
        let mut reopened = MemoryService::at_path(&dir, options).unwrap();
        let check_board = |board: &Board| {
            let pinned = board.template_instance.as_ref().expect("snapshot kept");
            assert_eq!(pinned.content_hash, instance.content_hash);
            assert_eq!(pinned.version, 1);
            let kinds: Vec<&str> = pinned
                .snapshot
                .presentation
                .components
                .iter()
                .filter_map(|c| c["kind"].as_str())
                .collect();
            assert!(kinds.contains(&"kanban_future"), "{kinds:?}");
            let owner = &pinned.snapshot.post_templates[0].schema.fields[3];
            assert_eq!(owner.extra.get("x_hint"), Some(&json!("assignee")));
            assert_eq!(board.fields.as_ref(), Some(&project_fields()));
            assert_eq!(board.links, links);
        };
        check_board(
            &reopened
                .board_info(&board_id, OWNER, WS, None)
                .unwrap()
                .unwrap(),
        );
        let check_post = |post: &BoardPost| {
            assert_eq!(post.post_template_id.as_deref(), Some("task"));
            assert_eq!(post.post_template_version, Some(1));
            assert_eq!(post.fields.as_ref(), Some(&task_fields()));
            assert_eq!(post.links, links);
            assert_eq!(post.title.as_deref(), Some("Task"));
        };
        let posts = reopened
            .board_read(Some(&board_id), OWNER, WS, None, None, 20)
            .unwrap();
        check_post(&posts[0]);
        check_post(
            &reopened
                .board_get(Some(&board_id), OWNER, WS, None, &post_id)
                .unwrap()
                .unwrap(),
        );
        let detail = reopened
            .board_dashboard_detail(&board_id, OWNER, WS, None, 100)
            .unwrap()
            .unwrap();
        check_board(&detail.board);
        check_post(&detail.posts[0]);
        let listed = reopened.board_dashboard_list(OWNER, WS).unwrap();
        check_board(listed.iter().find(|b| b.board_id == board_id).unwrap());
        // Idempotent reopen by key after restart, and the post keeps validating.
        let again = open_project(&mut reopened, "launch", &instance, None).unwrap();
        assert_eq!(again.board_id, board_id);
        let next = post_task(
            &mut reopened,
            &board_id,
            Some(("task", 1)),
            Some(&task_fields()),
            "persist-2",
        )
        .unwrap();
        assert_eq!(next.sequence, 2);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
