import {
  isMemoryError,
  Memory,
  type MemoryError,
  type MemoryOptions,
} from "../index"

const projectOptions: MemoryOptions = { path: "./project" }
const client = new Memory(projectOptions)
const initializedClient: Memory = Memory.init({ path: "./new-project" })
const remoteClient = new Memory({ baseUrl: "http://127.0.0.1:8000" })
const session = client.session("alice", "conversation-1")

void client.add([{ role: "user", content: "Remember this." }], "alice", {
  requestId: "request-1",
})
void session.search("What did Alice say?", { topK: 5 })
void remoteClient.refresh()

function getMemoryErrorCode(error: unknown): MemoryError["code"] | undefined {
  return isMemoryError(error) ? error.code : undefined
}

void initializedClient
void getMemoryErrorCode
