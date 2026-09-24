# efibootmgr fixtures

`efibootmgr -v` output from a SolidRun R8000 (AMI firmware, efibootmgr 18).

- `r8000-slots.txt`: captured verbatim on 2026-09-24 after the boot-a/boot-b
  entries existed.
- `r8000-firmware-only.txt`: the same board before any Avocado entry existed,
  when firmware had made only its PXE entry and a short-form `UEFI OS` entry.
  Transcribed from the raw efivarfs variables into efibootmgr's text shape.
- `r8000-after-create-only-boot-b.txt`: what `efibootmgr -C` (create-only)
  for boot-b leaves on that board. Constructed: the new entry appears and
  BootOrder is untouched.
