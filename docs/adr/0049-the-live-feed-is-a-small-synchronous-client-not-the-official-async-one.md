# 0049. The live feed is a small synchronous client, not the official async one

- Status: Accepted
- Date: 2026-10-06
- Jira: TIC-50

## Context

ADR 0038 kept the async runtime out of the engine side and left the live client as a separate decision.
Databento's official Rust client (`databento`) is built on tokio. The live gateway's protocol is a few lines of
text and then the DBN stream the decoder already reads. The engine needs three things from the feed: it must
never wait for the engine (ADR 0039), it must name instruments and keep their ids across a reconnect, and a
silent session must be noticed.

## Decision

`tf-live` implements the gateway's protocol itself, over `std::net::TcpStream` and one reader thread.

- **Why not the official crate.** It would bring an async runtime into the process to read one socket,
  and the thing to reason about, a bounded queue the engine reads at its own pace, is simpler with a thread that
  blocks on read and offers each event to a queue that never blocks it. The protocol was checked
  against the official client's source (login hash, line formats, chunking, resume) and the real gateway's
  greeting and login answer.
- **Stall detection by read timeout.** The login asks for a heartbeat every 5 seconds; a read that times
  out after 20 seconds (configurable) means no data and no heartbeat, and the session is dead. A partial
  record is never left half-read: the timeout ends the session.
- **Resume by a new session.** `reconnect(resume_from)` logs in again and re-sends every subscription
  with `start=resume_from`. The gateway's boundary event can come twice (same time, same sequence); the
  caller dedupes. Instruments keep their dense ids because the numbering moves from one session's
  decoder to the next. Sharding and universe changes mid-session are E06-S03.
- **The skip notice is a gap.** A gateway's "skipped records after slow reading" becomes a `Skipped`
  marker in the queue, in order with the events, so the engine and the report know the stream is not whole.
- **SHA-256 is written here**, because the login needs exactly one hash and the tests carry the standard's
  vectors; a general crypto dependency is not wanted for that.
- **The key** is read from a file by the caller, never put in an argument or a log; its `Debug` shows the
  last five characters, which the protocol sends anyway as the bucket.

## Consequences

- No tokio in the process. A thread per session (up to ten on the Standard plan).
- What is confirmed against the real gateway is the greeting, the challenge and the login line (it answered
  with a licence error, which it could only give after reading the login). The subscription, the stream,
  heartbeats and replay are confirmed only against a fake gateway until a live plan exists; the first
  paid session is also the first test of those, which is why the driver (E18-S12) is built to stop
  cleanly and say what it saw.
