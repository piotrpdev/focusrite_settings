# FocusriteUsbSwRoot.sys — ioctl surface

The software root enumerator owns the `ROOT\FOCUSRITEUSBNEW\0000` device whose `\pal`
interface Pal opens. This is the driver that answers the ioctl codes Pal sends; the USB
function driver (`FocusriteUsb.sys`) does not contain them at all.

- sha256 `cb377ef0a77410afe34290fb9bb1356be312a56a83d9b15ee2403961d721421d`
- image base `0x140000000`, 326 procedures, 273 strings
- file version `4.143.0.261` (same as every driver in this package)

## 1. Where the ioctl codes live

`FocusriteUsb.sys` contains **no** occurrence of the bytes `00 20 22 00` (`0x222000`) or
`08 20 22 00` (`0x222008`), which is why earlier passes recorded the dispatch as untraced.
Searching the sibling drivers finds them:

| file | `0x222000` occurrences |
|---|---|
| `FocusriteUsbSwRoot.sys` | 1 (`0x140010996`) |
| `FocusritePCIeSwRoot.sys` | 1 |
| `FocusriteUsbMidi.sys` | 1 |
| `FocusritePCIe.sys` | 2 |
| `FocusritePCIeMidi.sys` | 1 |
| `FocusriteUsb.sys`, `FocusriteUsbAudio.sys`, `FocusritePCIeAudio.sys` | 0 |

`0x222008` is not present as a literal anywhere: the handler compares the whole
`IoControlCode` value, and the compiler emitted one comparison per code (see below).

## 2. The handler

`FUN_140010870` — signature `(driver-globals, request, outLen, outPtr, ioctlCode)` — is the
`EvtIoDeviceControl` dispatch for the `\pal` device. It fetches the request's output buffer
through the KMDF function table (`WdfFunctions + 0x868/0x870` slot calls, i.e. KMDF import
via `WDFLDR` with no named import, which is why name-based xrefs do not resolve here) and
then compares:

```
param_5 == 0x222000 -> version descriptor  (see §3)
param_5 == 0x222004 -> {u32 1, u32 <virtual call result>}
param_5 == 0x222008 -> DICE command path
param_5 == 0x22200c -> notification read
```

Otherwise it completes the request with `0xC0000023` (`STATUS_BUFFER_TOO_SMALL`) when the
output pointer is null or `param_3 < required`, which is the same status Pal's failures show
for a short buffer.

## 3. The 16-byte version descriptor

For `0x222000`, when the output buffer is present and at least `0x10` bytes
(`param_3 < 0x10` is rejected), the handler writes four **hard-coded** dwords:

```c
*out       = 0x7000;      // word 0
out[1]     = 0x7000;      // word 1
out[2]     = 0x105;       // word 2
out[3]     = 0x8048F000;  // word 3
```

(REA address `0x1400118d3`–`0x1400118dd`, pseudocode `*local_a8 = 0x7000; local_a8[1] =
0x7000; local_a8[2] = 0x105; local_a8[3] = 0x8048f000;`.)

They are immediates in `.text`, not a data table: there is no 16-byte copy of the sequence
anywhere in the file. `FocusritePCIeSwRoot.sys` writes the same four values, so the
descriptor is a shared constant of the software-root layer, not per-device data.

Reading of each word, from the consumer side (`FocusritePal64.dll`, `0x18000BE70`):

| word | value | meaning | evidence |
|---|---|---|---|
| 0 | `0x7000` | driver's own API version | not compared; only printed by Pal's mismatch message as "Driver minimum" |
| 1 | `0x7000` | minimum Pal API the driver accepts | `cmp dword ptr [buf+4], 0x7000; jbe ok` — Pal throws when this is **greater** than its own `0x7000` |
| 2 | `0x105` | build number | equals `261`, the revision component of the package's `4.143.0.261` file version, for every driver here |
| 3 | `0x8048F000` | unknown build stamp | not the PE `TimeDateStamp` (`0x68A71E27`), checksum (`0x26B9A`), image size or CRC-32; Pal stores it and never reads it |

Words 2 and 3 are copied by Pal as one 64-bit value into its device object at `+0x1A8`;
no function in the DLL reads that field back (only the store at `0x18000c0eb` and an
unrelated initialiser in `FUN_180009e50` reference that displacement).

## 4. Not resolved

* What `0x8048F000` encodes. It is identical in the USB and PCIe software-root drivers, is
  not derived from any header field of either binary, and Pal ignores it.
* Whether the word 0 / word 1 labelling in Pal's message ("Driver minimum") is meant for
  word 0 or word 1; the comparison uses word 1, the message prints word 0, and both are
  `0x7000` in this build so the discrepancy is invisible.
* The `{0x400, N}` command dispatch itself lives behind `0x222008` in this handler but was
  not traced further; the wire framing is known from Pal and verified end-to-end.
