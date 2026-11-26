---
status: accepted
decided: 2026-11-26
updated: 2026-11-26
---

# Orchestrate existing servers instead of shipping one

Decided in the PRD, [#4](https://github.com/danielcopper/copper-dictate/issues/4) (D2).

## Context

A dictation with a live preview uses two models: a fast streaming one for the live text while the user speaks, and an
accurate one for the final text when they stop. A self-hoster may already run a transcription server, such as an
OpenAI-compatible Whisper server on a GPU, and wants the client to use it rather than run a second Whisper beside it
(#4, user story 30).

## Decision

copper-dictate ships no server code of its own. The client orchestrates two existing servers: the final text comes from
an OpenAI-compatible `/v1/audio/transcriptions` endpoint, the live text from a realtime WebSocket server. Which models
they serve is configuration, not code.

## Considered options

- **Orchestrate existing servers** (chosen). The client connects to the final server and the live server the user
  configures; the final server can be an OpenAI-compatible transcription server the user already runs.
- **A server of its own**, shipped with the client, serving both models. It would sit beside the transcription server a
  self-hoster already runs, a second Whisper next to the first, and copper-dictate would maintain the model serving
  itself.
- **RealtimeSTT** as the server, with its live and final models. Not adopted: it loads its own Whisper on the GPU beside
  the existing one, sends no prompt to its final model (so no vocabulary prompt reaches it), and cannot call an external
  final server.

## Consequences

- The client speaks the two servers' protocols and follows their changes. Its tests run against small local fake servers
  that stand in for them; no CI test runs against real ones (#4, D13).
- The client builds the vocabulary prompt from the term list and sends it with the final request; the filter for a
  repeated last sentence and the replacement list run in the client.
- The live server is a second server to run, so the live preview is optional: with only a final server, the client still
  dictates.
- Someone without servers of their own is to start them from the reference compose files the repository will ship (#4,
  D11); operating the servers is not part of the client.
