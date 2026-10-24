# Diagram prompt — how `focusrite_settings` works

A paste-ready prompt for an image model: one poster that shows the whole run from
argv to exit code, with the ioctl mechanism in the middle.

The `docs/diagram.jpg` in this repository was generated from it with Nano Banana 2
(Gemini 3.1 Flash Image).

Every number, path and id in it comes from this crate (`src/protocol.rs`,
`src/cli.rs`, `src/win/`) and from `docs/FocusritePal64.dll.md` /
`docs/FocusriteUsbSwRoot.sys.md`. If the code changes, the prompt should change
with it.

Facts the poster has to get right, in case you edit the prompt:

- `0x222000` and `0x222008` are the **ioctl codes** — `DeviceIoControl`'s second
  argument; the driver switches on them to pick a handler.
- `1 / 2 / 4 / 5 / 9` are **command ids** inside the `0x222008` payload, next to
  `section 0x400`. They are not ioctls, and they never appear in the ioctl code.
- The ioctl code is an argument to the call. It is not part of the buffer.
- The tool's own trace prints section and command id (`[0x400/0x9]`), not the
  ioctl code.

## Prompt

```text
A single technical infographic poster, flat vector style, dark slate background
(#12161c) with a thin cool-grey hairline grid, monospace labels, no photographic
elements, one soft amber accent glow behind the driver panel. Title top-left in
large monospace: "focusrite_settings". Subtitle beneath, smaller: "sets the
buffer size and sample rate of a Focusrite interface by writing ioctl requests
straight to the driver". Boxes, arrows and short labels only; no paragraphs.

CANVAS: 16:9 landscape. Four horizontal zones, top to bottom, heights roughly
16% / 16% / 50% / 14%, with the remaining space for the title. Arrow flow: down
the left edge between zones, rightward inside each zone. Zone 3 gets the most
room and is divided into sub-panels labelled 3a (left), 3a2 (key, directly under
3a), 3b (centre), 3c (right) and 3d (a strip along the bottom of the zone).

ZONE 1 - "1. COMMAND LINE" (blue accent)
- Terminal box with a caret prompt containing exactly:
  focusrite_settings rate 48000 buffer 256
- Arrow to a box labelled "parse arguments", then three branch arrows:
  a) "bad value -> usage, exit 2"
  b) "--help -> usage, exit 0"
  c) down to "rate 48000 Hz, buffer 256 samples"
- Grey footnote box: "values outside 16-1024 / 44100-192000 still sent, with a
  note"

ZONE 2 - "2. FIND THE DEVICE, OPEN A HANDLE" (teal accent)
- "HKLM ... DeviceClasses" -> "class GUIDs with a Focusrite \pal instance"
- small stacked cards labelled "+ 3 fallback GUIDs", one card reading
  AC4D0455-50D7-4498
- -> "SetupDiGetClassDevsW, enumerate interfaces"
- -> one example path in small monospace:
  \\?\root#focusriteusbnew#0000#{ac4d0455-...}\pal
- -> "order: \pal first, then vid_1235"
- -> "CreateFileW, share mode 0, FILE_FLAG_OVERLAPPED" with three chips beside
  it reading "rw", "rw-data", "none" and the note "error 32 -> close Notifier /
  Focusrite Control"
- From the CreateFileW box a thick arrow down into zone 3 labelled "handle,
  then ioctls only" - the handle is the only thing handed to zone 3.

ZONE 3 - "3. TALK TO THE DRIVER: ONE HANDLE, TWO IOCTL CODES" (amber accent)
One-line definition box across the top of the zone, two short lines:
  "ioctl = a control request sent down an open device handle"
  "two numberings: the ioctl code chooses the driver handler, the section+id
   inside the payload chooses the command"

3a - "THE CALL" (left sub-panel)
- A function signature drawn as eight stacked labelled slots, header
  "DeviceIoControl":
  handle | code | in buffer | in length | out buffer | out length |
  bytes returned | OVERLAPPED
- The "code" slot is highlighted and fans out into exactly two chips:
  "0x222000" and "0x222008"
- Under the chips, one short caption: "the ioctl code is an argument to the
  call, it is never inside the buffer"
- Note under the signature: "same wrapper for both codes: ioctl_xfer"

3a2 - "WHICH NUMBERING" (small high-contrast key panel under 3a)
Two stacked rows, each a badge plus an arrow plus a label:
  row 1: badge "0x222000" -> "one-shot: get the 16-byte descriptor"
  row 2: badge "0x222008" -> "command handler" -> small funnel -> badge
         "section 0x400 + id" -> "9 commands: 1 2 4 5 9"
Beneath the rows, one underlined caption:
  "the ids on the pills are command ids, not ioctl codes"

3b - "THE BYTES" (centre sub-panel)
- Upper strip, title "request, sent through ioctl 0x222008", a 5-segment bar:
  "token 0 (8 B)" | "section 0x400 (2 B)" | "id (2 B)" | "reserved 0 (4 B)" |
  "payload"
  the first four segments bracketed together as "16-byte header"; the "id"
  segment outlined in the amber accent with a pointer down to "command id:
  1 2 4 5 9"; the payload segment annotated "4 B u32 LE for ids 5 and 9,
  empty for ids 1, 2 and 4"
- Lower strip, title "response", a 3-segment bar:
  "status (4 B)" | "length (4 B)" | "data"
  annotated "status 0 = ok, otherwise the driver rejected it" and
  "length is always 0, so the tool asks for 8 + expected and ignores it"
- Monospace footnote with a small bracket under the section and id parts:
  "[0x400/0x9] status=0 len=0 io=8" captioned "what the tool prints:
  section / command id"

3c - "THE WAIT" (right sub-panel, vertical timeline)
- t=0 "DeviceIoControl returns", two branches:
  "TRUE -> done, read bytes" and "FALSE + 997 ERROR_IO_PENDING -> wait"
- -> "WaitForSingleObject(event, 5000)"
- two outcomes:
  "signalled -> GetOverlappedResult -> bytes" with a green tick
  "silent at 5 s -> CancelIo" -> "wait up to 2000 ms more" ->
  "GetOverlappedResult drains it" -> "report timeout"
- dashed red exit from the drain: "never completes -> abort", annotated
  "the output buffer must outlive the request"
- caption: "CancelIo only asks; the buffer stays alive until the driver
  answers"

3d - command strip along the bottom of zone 3
- The whole strip is enclosed in one large rounded container whose tab label
  reads exactly: "ioctl 0x222008 - one call per pill"
- Inside, nine numbered pills connected left to right:
  1 "attach" | 2 "read config -> 0x58 B" | 3 "set rate (u32 Hz)" |
  4 "wait 250 ms" | 5 "set buffer (u32 samples)" | 6 "wait 250 ms" |
  7 "read config -> 0x58 B" | 8 "compare" | 9 "detach"
  each pill carries a small corner badge with its command id: "id 1", "id 2",
  "id 4", "id 5", "id 9", "id 4", "id 2"
- pill 9 dashed, labelled "always runs, even on error"
- under pills 2 and 7: "rate at +0x0C, buffer at +0x14"
- OUTSIDE the container, to its left, one separate call box on its own arrow:
  "ioctl 0x222000" -> a 4-cell card reading
  "api 0x7000" | "needs Pal <= 0x7000" | "build 261" | "stamp 0x8048F000"
  -> red-edged gate box "driver wants newer Pal -> exit 1"
  captioned "no payload, one shot, before any command"

ZONE 4 - "4. RESULT" (grey accent)
Four square badges in a row:
"0 verified" | "1 driver error" | "2 usage" | "3 read-back differs"

RIGHT EDGE, a narrow muted panel titled "not used":
"FocusritePal64.dll", "Focusrite Control", "audio streams",
"ioctl 0x22200C (notifications)"

ARROWS: solid light-grey for the happy path; dashed red only for the three
failure exits (error 32, the Pal gate, abort). Orthogonal routing, right angles,
no curves except the accent glow.

TEXT TO RENDER EXACTLY, short monospace strings, no invented words:
focusrite_settings / DeviceIoControl / ioctl_xfer / 0x222000 / 0x222008 /
0x400 / 0x400/0x9 / section 0x400 + id / token 0 / reserved 0 / status /
length / 0x58 / +0x0C / +0x14 / u32 LE / OVERLAPPED / 997 / 5000 ms /
2000 ms / 250 ms / 0x7000 / 0x8048F000 / AC4D0455-50D7-4498 /
\\?\root#focusriteusbnew#0000\pal / id 1 / id 2 / id 4 / id 5 / id 9 /
ioctl 0x222008 - one call per pill / exit 0 1 2 3

STYLE: crisp 1px strokes, 2px rounded corners on boxes, small caps zone
headers, generous whitespace, no drop shadows except a faint one under the 3b
byte bars, no logos, no brand marks, no human figures.
```

## Notes when regenerating

- The two container labels (`ioctl 0x222008 — one call per pill`, `ioctl
  0x222000`) and the two chips (`0x222000`, `0x222008`) carry the ioctl-vs-command
  distinction. Protect them first.
- Second priority: `997`, `5000 ms`, `2000 ms` in 3c, and the four header fields
  in 3b — those are what make the mechanism legible rather than magic.
- Safe to lose if text overflows: the per-pill `id N` badges, the `+0x0C/+0x14`
  annotations, the "not used" side panel, and the `0x58`/`u32 LE` details.
- If a constant comes out wrong, regenerate rather than retouching: a wrong
  `0x222008` is worse than a missing one.
