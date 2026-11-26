# copper-dictate

Self-hosted live dictation for Linux desktops: live text while the user speaks, an accurate final text when they stop.
This glossary fixes what the project's terms mean; the decisions behind them are in `docs/adr/` and the issues.

## Language

### Dictation

**Dictation**:\
Speech turned into text at the cursor. A dictation is one run from start to stop, with its recording, its final text
and, with the live preview, its live text; a cancel discards it.

**Live text**:\
The provisional text the live server returns while the user speaks; it may still change.\
_Avoid_: partial result, interim text

**Live preview**:\
The live text, shown in the overlay while the user speaks. It is optional: without it, a dictation still ends in its
final text.\
_Avoid_: live transcript

**Final text**:\
The accurate text of a whole recording, computed once by the final server when the user stops. It is the text inserted
at the cursor.\
_Avoid_: final transcript, result

### Servers

**Live server**:\
The existing speech server that turns the audio into live text while the user speaks. Its address in the configuration
is the live endpoint.\
_Avoid_: preview server, streaming server

**Final server**:\
The existing transcription server that turns the whole recording into the final text. Its address in the configuration
is the final endpoint.\
_Avoid_: Whisper server

### Client

**Core**:\
The platform-independent part of the client that runs a dictation: audio and start, stop and cancel go in; what the
overlay shows and the text to insert come out. The counterpart of the platform parts.

**Platform part**:\
A part of the client that depends on the desktop it runs on, such as the shortcut, the insertion at the cursor and the
overlay. Each stands behind an interface, so another desktop gets its own without a change to the core.\
_Avoid_: adapter, integration, plugin

**Overlay**:\
The small window that shows a dictation while it runs: that it is recording, the live preview, or the notice that the
final text follows. It takes neither focus nor clicks.\
_Avoid_: popup, HUD

### Spelling

**Term list**:\
The user's own words, such as names and technical terms, kept in the configuration and spelled as they must appear in
the final text.\
_Avoid_: dictionary, hotwords, word list

**Vocabulary prompt**:\
A natural sentence built from the term list that goes to the final server with the recording, so that the final text
spells those terms right.\
_Avoid_: initial prompt, context prompt

**Replacement list**:\
Pairs kept in the configuration, each a recurring misrecognition and the text that replaces it in the final text; it
fixes what the vocabulary prompt cannot.\
_Avoid_: substitution list, autocorrect
