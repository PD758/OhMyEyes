# Contributing

Contributions are welcome while the project is evolving. Open an issue before a large architectural change so platform constraints and scope can be agreed first.

## Quality bar

- Keep platform-specific APIs behind a narrow module boundary.
- Keep scheduler and configuration behavior deterministic and unit-tested.
- Do not add a background service, elevation requirement, telemetry, or network access.
- Avoid panics on user-controlled files or configuration.
- Preserve click-through behavior and explicit user consent for autostart.
- Add dependencies only when their maintenance and platform cost are justified.

Before opening a pull request, run:

```powershell
cargo fmt --all -- --check
.\build-local.bat clippy
.\build-local.bat test
```

Windows UI smoke tests require an interactive desktop session and no other running OhMyEyes instance:

```powershell
.\scripts\smoke-test-settings.ps1
.\scripts\smoke-test-overlay.ps1
```

Use focused commits and describe observable behavior, platform coverage, and remaining test gaps in the pull request.
