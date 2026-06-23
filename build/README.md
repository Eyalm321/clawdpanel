# Build Assets & Configuration

This directory contains resources, templates, and scripts used for packaging the compiled Rust binaries into installers and distribution packages.

## Directory Structure

*   `darwin/` - macOS-specific packaging assets.
    *   `Info.plist` - Plist metadata used for the macOS `.app` bundle.
    *   `icons.icns` - Application icon for macOS.
    *   `Assets.car` - Compiled asset catalog.
    *   `scripts/` - Installation and setup scripts executed by `pkgbuild`.
*   `windows/` - Windows-specific packaging assets.
    *   `icon.ico` - Application icon for Windows.
    *   `installer/` - Files used to create the Windows NSIS installer:
        *   `project.nsi` - Main NSIS installer script.
        *   `wails_tools.nsh` - Included helper macros for file registration and installation.
        *   `configure-statusline.ps1` - PowerShell script to configure Claude Code's statusline integration.
*   `linux/` - Linux-specific packaging assets (`nfpm.yaml`, desktop entry, and installer scripts).