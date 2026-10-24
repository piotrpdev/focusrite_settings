# Focusrite Notifier — "Buffer Size" dropdown: end-to-end trace

Artifact: `Drivers/Focusrite Notifier.exe`
sha256: `8e75ab9794c67e2d172c7b8987c6505d2647640ac0fb903ae2e82621e1a79c89`
Image base `0x140000000`. All addresses below are live (Ghidra) addresses in that image.
Symbols are stripped; names are `FUN_*` from Ghidra except imported MFC/CRT functions.

Confidence tags: **[obs]** = read directly from the binary; **[inf]** = inferred from layout/semantics
(usually because the implementation lives in `FocusritePal64.dll` or the kernel driver;
the Pal side is now documented in `FocusritePal64.dll.md`).

---

## 1. Menu item → window

* Tray right-click handler `FUN_14000b2b0` (tray-window message `0x401`) builds the popup menu with
  `CreatePopupMenu`/`AppendMenuA`. **[obs]**
  For each enumerated device it appends an item whose **command id is `40000 + deviceIndex`** and
  whose text is `<device name>` + `" settings"` (the bytes `0x20 73 65 74 74 69 6e 67 73` are memcpy'd
  onto the name). Note: the *menu* says `... settings`; the *window title* says `... Device Settings`.
* `WM_COMMAND` in the range `0x9C40..0x9C43` (40000..40003) maps to `FUN_14000b980`, which posts a
  thread message `0x403` with `wParam = id - 40000` (device index) and `lParam =` packed screen coords.
  **[obs]** (message-map entry at `0x14003e620`)
* `CTrayparkerApp` message pump override `FUN_14001b690` handles `0x403`/`0x404` (and otherwise defers to
  `CWinThread::PreTranslateMessage`). **[obs]**
* `FUN_14001b040` handles `0x403`: looks the device up by index (`FUN_14001b830` on a map at `app+0x1d0`),
  bails out to the "control software" path if the device is not ready (`FUN_14001f2d0`), otherwise
  `new`(0x9e8) `CTrayparkerDlg` (`FUN_14001bad0`), calls virtual slot `+0x2F8` (the modal
  `DoModal`), then destroys it. So the settings window is **modal and created per click**. **[obs]**

## 2. The settings window is dialog resource 102

`FUN_14001bad0` is the `CTrayparkerDlg` ctor and calls `CDialogEx::CDialogEx(this, 0x66 /*102*/, 0)`. **[obs]**

RT_DIALOG **102** (`.rsrc`, file offset `0xE13F8`, 1020 bytes) contains, among others: **[obs]**

| id | class | text |
|----|-------|------|
| 1002 | static | Product |
| 1004 | static | Serial |
| 1005 | static | Version |
| 1006 | combobox (CBS_DROPDOWNLIST) | (Sample Rate) |
| **1007** | **combobox (CBS_DROPDOWNLIST)** | **(Buffer Size)** |
| 1008 | combobox | (Clock Source) |
| 1013 | static | Locked |
| 1030 | button (auto-checkbox) | Safe Mode |
| 1032 | static | (Round-Trip Latency:) |
| 1036 / 1037 | combobox | In: / Out: |
| 1043 / 1044 | static | In: / Out: |

Control → member mapping is in `DoDataExchange` `FUN_14001bde0`: `DDX_Control(pDX, 0x3EF /*1007*/, this+0x270)`;
the HWND sits at `+0x2B0` (`CWnd::m_hWnd` is `+0x40` in MFC). **[obs]**

`OnInitDialog` `FUN_14001bf20`: **[obs]**
* `Format("%s Device Settings", this->deviceName@+0x818)` → `SetWindowTextA` (this is the title you see;
  `+0x818` is a copy of the device's model-name pointer, e.g. `"Scarlett Solo (3rd Gen)"`).
* `ShowWindow(SW_SHOW)`.
* `FUN_14000c000(this, deviceSharedPtr, 0)` → writes a `*_stats.log` snapshot next to the logs.
* `FUN_14001f390(deviceDelegate, this)` → **registers this dialog HWND in the device's window vector**
  (`std::vector<HWND>` at delegate `+0xC0`) so it can be refreshed by broadcast later.

## 3. Where the dropdown's items come from

`FUN_14001c2f0` (handler for the custom "refresh" message `0x402`) fills the combos from three
`std::map<int,std::string>` caches inside the device delegate: **[obs]**

| Combo | control | delegate map | current value (dialog) | delegate field |
|---|---|---|---|---|
| Sample Rate | 1006 | `delegate+0x158` | `+0x800` | `+0x18` |
| **Buffer Size** | **1007** | **`delegate+0x168`** | **`+0x804`** | **`+0x1C`** |
| Clock Source | 1008 | `delegate+0x178` | `+0x808` | `+0x20` |

For each map entry it does `CB_ADDSTRING(0x14A)` with the mapped string and
`CB_SETITEMDATA(0x151)` with the **integer key**. The item that equals the current value gets
`CB_SETCURSEL(0x14E)`. **[obs]**

The buffer-size map `+0x168` is filled in the delegate refresh `FUN_14001f7b0` from a **hard-coded
list** — `16, 32, 48, 64, 96, 128, 160, 192, 256, 512, 1024` samples — each inserted with an
itoa-style string (`FUN_140003f00`), i.e. the combo shows plain decimal sample counts. **[obs]**
So the Notifier does not query the driver for the buffer-size list; it offers that fixed table.

## 4. What changing the value does

Message-map entry at `0x14003F6B0` = `{ WM_COMMAND, CBN_CLOSEUP(8), 1007, 1007, sig, pfn }` →
**`FUN_14001cdd0`**. **[obs]** (so the action happens when the drop-down closes on a selection)

`FUN_14001cdd0`: **[obs]**

```c
idx   = SendMessage(combo1007, CB_GETCURSEL /*0x147*/, 0, 0);
size  = SendMessage(combo1007, CB_GETITEMDATA /*0x150*/, idx, 0);   // samples
if (size == this->currentBufferSize /*+0x804*/) return;             // no-op if unchanged
EnableWindow(combo1007 /*this+0x270*/, FALSE);                     // lock UI until confirmed
palDevice = *(PalDevice**)this->deviceSharedPtr;                    // shared_ptr target, +8 field
(*(*palDevice + 0x160))(palDevice, size);                           // <-- the actual set
```

`palDevice` is the app's `Pal::Device`-style interface object obtained through
`FOCUSRITEPAL64.DLL::Pal::System::createSystem` (the delegate stores it at `+8`, constructor
`FUN_14001e110` does `pal->setDelegate(this)` via vtable `+8`). **[obs for the object/layout]**

* Slot `+0x160` is called with a single `unsigned` = the selected sample count. **[obs]**
* That slot is the **setter** counterpart of the getter at `+0x158`, which the delegate uses as
  "get current buffer size" (`FUN_14001f7b0` stores its result at `delegate+0x1C`, and callback
  `FUN_1400200c0` likewise). **[obs for the getter, inf for the setter name]**
* Sibling handlers do the same for the other combos: sample rate → slot `+0x40` (`FUN_14001cd40`),
  clock source → slot `+0x50` (`FUN_14001ce60`). **[obs]**

What `+0x160` actually is (resolved against `FocusritePal64.dll`, see
`FocusritePal64.dll.md`): the abstract `Pal::Device` vtable slot is pure, and the concrete
`Pal::DiceDevice` implementation is `Pal!0x180008BA0`, which sends the DICE command
`section 0x400, id 9` with the 4-byte sample count. That command goes to
`Pal::DiceRootDriverWin::vtbl[0x18]` (`Pal!0x18000C6C0`) → `ioctl()` (`Pal!0x18000CD00`) →
`DeviceIoControl(hFocusriteUsb, 0x222008, …)` (METHOD_BUFFERED) on the handle returned by
`CreateFileA` in `Pal!0x18000BE70`. The Notifier-side `+0x158` getter is the matching
getBufferSize (`*(int*)(driver+0x24)`). **[obs]**
Whether the driver persists the value across reboot/ASIO sessions is still outside both binaries.

## 5. How the change comes back (and what triggers the notification)

The delegate implements `Pal::DeviceDelegate` (`CWinUsbDeviceDelegate`, RTTI at `0x14004FC88`,
vtable `0x1400411D8`). Its vtable slots 4..7 are per-setting "changed" callbacks; each **re-reads the
value through the Pal getter**, caches it, and broadcasts a bit: **[obs]**

| slot | fn | reads | caches | broadcast flag |
|---|---|---|---|---|
| `+0x20` | `FUN_140020000` | `pal+0x48` (sample rate) | `delegate+0x18` | `1<<3 = 0x08` |
| `+0x28` | `FUN_140020030` | `pal+0x58` (clock) | `+0x20` | `1<<4 = 0x10` |
| `+0x30` | `FUN_140020060` | `pal+0x58`/`+0x60` (clock/sync) | `+0x20`,`+0x24` | `1<<5 = 0x20` |
| **`+0x38`** | **`FUN_1400200C0`** | **`pal+0x158` (buffer size)** | **`delegate+0x1C`** | **`1<<6 = 0x40`** |

`FUN_14001ff50` → `FUN_14001fe00` then posts **`0x402`** (`WM_APP+2`) with
`wParam = deviceIndex`, `lParam = flags` to every HWND in the delegate's vector at `+0xC0` —
i.e. **both the tray window and any open settings window**. **[obs]**
(There is also a threadpool timer on the delegate, `FUN_14001fe90`, that rebroadcasts cached state;
it uses the same path.)

Confirmation from the Pal side: `Pal!FUN_18000AB30` compares the driver's cached values
(`+0x18` sample rate, `+0x1C` clock, `+0x24` buffer size) against snapshots (`+0x70/+0x74/+0x7C`)
and calls `delegate->vtbl[+0x20]`, `[+0x28]` and `[+0x38]` respectively — which are exactly the
Notifier's slots 4, 5 and 7. So slot 7 really is `Pal::DeviceDelegate::bufferSizeChanged()`
(the name is in the import table and RTTI of the DLL). **[obs]**

## 6. The toast you see

The tray window's `0x402` handler is `FUN_14000ba60` (map entry `0x14003E5E0`). For
`flags & (0x40|0x08|0x10|0x20)` it: **[obs]**

1. `FUN_14000cfc0` rebuilds the tooltip/balloon text from the **cached delegate state** (`+0x18`
   rate, `+0x1C` buffer, `+0x20` clock, channel counts via `FUN_140020ac0`, clock name via map
   `+0x178`, sync flag `+0x24`):
   `"%s %s\n  Sample Rate: %d (%d In/%d Out)\n  Buffer Size: %d\n  Clock: %s (%s)"`
   (format string at `0x14003DD60`).
2. Compares it with the previously cached string at tray `+0x388`; **only if the text changed** and the
   `preferences\notifications` setting is non-zero (default 1, `GetProfileInt(...,1)`) does it show the
   balloon: `Shell_NotifyIconA(NIM_MODIFY, ...)` with `uFlags = NIF_INFO (0x10)`,
   `szInfo = <that multi-line text>`, **empty `szInfoTitle`** (so the OS shows "Focusrite Notifier"),
   `dwInfoFlags = NIIF_INFO (1)`.
3. Always updates `szTip` with the same text (`uFlags = NIF_ICON|NIF_TIP`), which is why hovering the
   tray icon shows the new buffer size too.

The same `0x402` broadcast hits the settings dialog, which re-runs `FUN_14001c2f0`: reloads the state,
re-populates and re-selects the combos, **`EnableWindow(combo1007, TRUE)`** (undoing the lock from
step 4), and recomputes the "Round-Trip Latency" label through `FUN_14001cc30`:
`ceil(10000 * (inSamples+outSamples) / sampleRate) / 10` ms, printed as `"%3.1fms - (%d samples)"`.
The two sample counts come from a Pal property read (`pal+0x48`, opcode `0x401`, property `0xD`,
20-byte struct seeded with the current rate/buffer). **[obs]**

## 7. Sequence (happy path)

```
tray WndProc 0x401 → FUN_14000b2b0 → menu item id 40000+i, text "<model> settings"
WM_COMMAND 0x9C40+i → FUN_14000b980 → PostThreadMessage(0x403, i, coords)
FUN_14001b690 (0x403) → FUN_14001b040 → new CTrayparkerDlg → DoModal
  OnInitDialog: title "<model> Device Settings", register HWND with delegate
user picks buffer size in combo 1007 → CBN_CLOSEUP
FUN_14001cdd0: CB_GETCURSEL + CB_GETITEMDATA → (pal->vtbl[0x160])(pal, samples); combo disabled
  = Pal!0x180008BA0 setBufferSize → driver cmd {0x400, 9, u32 samples}
  → Pal!0x18000C6C0 → Pal!0x18000CD00 → DeviceIoControl(FocusriteUsb, 0x222008, METHOD_BUFFERED)
Pal notification thread Pal!0x18000D1D0:
  DeviceIoControl(FocusriteUsb, 0x22200C, …) → DiceDevice::onNotification (Pal!0x1800087C0)
  → Pal!0x18000AB30 sees driver+0x24 != +0x7C → delegate->vtbl[0x38] (bufferSizeChanged)
  = Notifier 0x1400200C0 → FUN_14001ff50(6) → FUN_14001fe00 → PostMessage(0x402, devIdx, 0x40)
tray FUN_14000ba60: rebuild text → changed? → Shell_NotifyIconA(NIM_MODIFY, NIF_INFO) toast
dialog FUN_14001c2f0: repopulate combos, re-enable combo 1007, recompute latency label
```

## 8. State / persistence

* The Notifier keeps no copy of the buffer size other than the delegate's cache (`+0x1C`) and the
  dialog's snapshot (`+0x804`); it always re-reads it through Pal after a change. **[obs]**
* No registry/INI write of the buffer size exists in this binary; the only `preferences` values
  written are `screenPos` (window placement) and `notifications` (toggle). **[obs: only those
  `AfxGetModuleState()->WriteProfile*` call sites were found around this feature]**
* Device model names are resolved from a hard-coded PID→name map in the delegate ctor
  (`0x12358211` = "Scarlett Solo (3rd Gen)", `FUN_14001e110`). **[obs]**

## 9. Residual unknowns (bounded)

* Pal slot semantics are now established from `FocusritePal64.dll` (see
  `FocusritePal64.dll.md`), including the DICE command framing and the ioctls
  (`0x222000` version, `0x222008` command, `0x22200C` notification wait).
* Not resolved: the device-interface GUID / `\\?\` path Pal passes to `CreateFileA`; the driver-side
  meaning of `section 0x400` / ids `1,2,5,6,9`; and the property ids passed through
  `Pal::Device+0x138` (`0xD`, `0x12`, `0x18`, `0x1A`, `0x1E`, `0x1F`, `0x20`, `0x23`).
* `FocusriteUsb.sys` is KMDF (`WDFLDR` imports) and contains no literal `0x222008` / `0x22200C`
  constants, so its ioctl dispatch was not traced in this pass.
* Whether the driver persists the buffer size across reboot/ASIO session is outside both binaries.
* The threadpool poll timer (`FUN_14001fe90`) exists but no `SetThreadpoolTimer` arming call was found
  in the Notifier besides the zero-interval stop in the destructor, so the change notification is
  driven by Pal's delegate callbacks, not by that poll. **[obs for the call sites]**
