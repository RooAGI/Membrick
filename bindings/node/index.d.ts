export interface MemoryOptions {
  /** Existing project root whose resident owns the memory service. */
  path?: string
  /** Remote Membrick HTTP service URL. Cannot be combined with path. */
  baseUrl?: string
  /** Optional bearer token for baseUrl. */
  apiKey?: string
  /** Request timeout in milliseconds. Defaults to 30000. */
  requestTimeoutMs?: number
  /** Override the bundled Membrick resident executable used for project connections. */
  residentExecutable?: string
}

export interface Message {
  role: string
  content: string
  timestamp?: number | null
  expires_at_ms?: number | null
  supersedes_id?: string | null
}

export interface MemoryRecord {
  id: string
  content: string
  role: string | null
  user_id: string
  session_id: string | null
  created_at: string | null
  expires_at_ms: number | null
  supersedes_id: string | null
  metadata: Record<string, string>
}

export interface SearchMemory {
  id: string
  content: string
  score: number
  created_at?: string
  role: string | null
  user_id: string | null
  session_id: string | null
  relation_evidence?: string[]
  comparison_operands?: string[]
  claim_states?: Array<Record<string, unknown>>
}

export interface AddOptions {
  requestId?: string
  sessionId?: string
  waitForVisibility?: boolean
}

export interface SearchOptions {
  topK?: number
  sessionId?: string
  filters?: Record<string, string>
  scope?: string
  referenceDate?: string
  options?: string[]
}

export interface ListOptions {
  sessionId?: string
  limit?: number
  cursor?: string
  includeInactive?: boolean
}

export interface UpdateOptions {
  role?: string
  timestamp?: number
  expiresAtMs?: number
}

export interface SessionSearchOptions extends Omit<SearchOptions, 'sessionId'> {}

export type MemoryErrorCode = 'REJECTED' | 'UNCERTAIN' | 'UNAVAILABLE' | 'CONFIGURATION'
export interface MemoryError extends Error {
  code: MemoryErrorCode
  status?: number
}
export declare function isMemoryError(value: unknown): value is MemoryError

export interface ListResponse {
  data: MemoryRecord[]
  next_cursor: string | null
}

export interface AddResponse {
  success: boolean
  request_id: string
  user_id: string
  session_id: string
  adjudication?: unknown
}

export interface AddTransactionRequest {
  request_id: string
  user_id: string
  session_id: string
  messages: Message[]
  metadata?: Record<string, unknown>
}

export declare class Memory {
  static init(options: MemoryOptions): Memory
  constructor(options: MemoryOptions)
  add(messages: Message[], userId: string, options?: AddOptions): Promise<AddResponse>
  addTransaction(requests: AddTransactionRequest[]): Promise<AddResponse[]>
  search(query: string, userId: string, options?: SearchOptions): Promise<SearchMemory[]>
  get(memoryId: string, userId: string, includeInactive?: boolean): Promise<MemoryRecord | null>
  list(userId: string, options?: ListOptions): Promise<ListResponse>
  update(memoryId: string, userId: string, content: string, options?: UpdateOptions): Promise<MemoryRecord | null>
  delete(memoryId: string, userId: string): Promise<boolean>
  refresh(): Promise<void>
  close(): void
  session(userId: string, sessionId: string): MemorySession
}

/** A client view that pins user and session identity across calls. */
export declare class MemorySession {
  readonly userId: string
  readonly sessionId: string
  close(): void
  add(messages: Message[], options?: Pick<AddOptions, 'requestId' | 'waitForVisibility'>): Promise<AddResponse>
  search(query: string, options?: SessionSearchOptions): Promise<SearchMemory[]>
}

export declare function version(): string
