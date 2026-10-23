# External test ROMs

GBA test suites from **jsmolka/gba-tests** (https://github.com/jsmolka/gba-tests),
used as the Phase 3 acceptance gate. Each ROM runs its suite and
shows the verdict on screen ("All tests passed" / "Failed test NNN").

| File | Suite |
|---|---|
| `arm.gba` | ARM instruction set (~550 tests) |
| `thumb.gba` | THUMB instruction set |
| `memory.gba` | Memory mirrors + video-memory byte stores |
| `flash64.gba` | Flash 64K save protocol |
| `flash128.gba` | Flash 128K save protocol |

Run one manually:

```sh
cargo run --release -p headless -- tests-roms/arm.gba 300
EMU_DUMP_FRAME=out.ppm cargo run --release -p headless -- tests-roms/arm.gba 300
python3 tests-roms/ppm2png.py out.ppm out.png   # view the verdict screen
```

Status at Phase 3: **all five suites pass.**
