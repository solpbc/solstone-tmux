# ELF fixtures for the static-linkage release gate

Real compiler output, committed so the release validator's ELF structure checks are exercised
against genuine header tables rather than hand-synthesized blobs. Hand-synthesis is how this gate
goes vacuously green: a zeroed 64-byte blob has `e_phnum = 0`, so "no `PT_INTERP`", "no
`DT_NEEDED`", and "no `DT_VERNEED`" all pass without any program header ever being parsed.

Do not regenerate or hand-edit these. Generated once per architecture with stock `gcc`
(x86_64 on an Ubuntu 24.04 host, aarch64 on an Ubuntu 24.04 aarch64 host) from the two sources
in this directory.

| fixture | built with | `e_type` | `e_entry` | `PT_INTERP` | `PT_LOAD` | `PT_DYNAMIC` | `DT_NEEDED` |
|---|---|---|---|---|---|---|---|
| `dynamic-interp.bin` | `gcc -o` | `ET_DYN` | non-zero | **present** | yes | present | non-zero |
| `static-pie.bin` | `gcc -static-pie -nostdlib -fPIE` | `ET_DYN` | non-zero | absent | yes | present | 0 |
| `static-exec.bin` | `gcc -static -nostdlib` | `ET_EXEC` | non-zero | absent | yes | **absent** | 0 |
| `relocatable.o` | `gcc -c` | `ET_REL` | 0 | absent | **no** | absent | 0 |
| `shared-nodeps.so` | `gcc -shared -fPIC -nostdlib` | `ET_DYN` | **0** | absent | yes | present | 0 |

## What each one is for

- **`static-pie.bin`** is the shape a `x86_64-unknown-linux-musl` release binary has. It must
  **pass**.
- **`static-exec.bin`** is the shape a `aarch64-unknown-linux-musl` release binary has — note it
  has **no `PT_DYNAMIC` at all**. It must **pass** on that lane. A gate that requires `PT_DYNAMIC`
  in order to prove `DT_NEEDED == 0` breaks this lane.
- **`dynamic-interp.bin`** is a normal dynamically-linked executable. It must be **rejected**, and
  it is the case the retired GLIBC-floor check let through whenever the host glibc was old enough.
- **`relocatable.o`** has no program headers at all. It must be **rejected** — and it is why the
  gate asserts `e_phnum > 0` and `PT_LOAD` rather than only asserting absences.
- **`shared-nodeps.so`** is the sharp one. On x86_64 it is **identical to `static-pie.bin` on every
  other assertion** — same `e_type` (`ET_DYN`), same machine, no `PT_INTERP`, a `PT_DYNAMIC`, and
  zero `DT_NEEDED`. **`e_entry` is the only discriminator**: a shared object has `e_entry == 0`,
  an executable does not. `DT_SONAME` is not usable here — `gcc -shared` without `-soname` omits it.

## Never execute these

They are fed to the ELF structure checks only. `static-exec.bin` and `static-pie.bin` are
`-nostdlib` stubs whose entry point is a trap instruction, and executing committed fixture bytes
is the same code-execution primitive the gate deliberately avoids by not shelling out to `ldd`.
