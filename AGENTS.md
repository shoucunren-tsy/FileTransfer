# Project Build Notes

- Keep the Windows MSI workflow free of custom WiX fragments unless they are validated with the exact Tauri WiX template.
- The application configures a best-effort Windows firewall port rule at startup; this avoids custom `light.exe` actions that can break MSI linking.
