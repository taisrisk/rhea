# Installing and updating (Windows)

The release file `Rhea-Setup-v<version>-windows-x64.exe` is its own installer.

When you run it from anywhere other than the install folder, it:

1. stops a running installed Rhea (never itself),
2. moves the old `Rhea.exe` aside and copies the new one to `%LOCALAPPDATA%\Programs\Rhea\Rhea.exe`, so an install over an existing one is an update,
3. adds a Start menu shortcut and an entry in *Settings > Apps* (uninstall string: `"…\Rhea.exe" --uninstall`),
4. starts the installed copy and exits.

If anything in that fails the app simply runs from where it is. Running from a cargo build folder (`target…`) is treated as development and never installs; set `RHEA_NO_INSTALL=1` or pass `--no-install` to skip the install step anywhere.

**Your data is separate from the program.** Chats, settings, skills, the local model and the Chrome extension folder live in `~/.rhea`; credentials live in the Windows Credential Manager. Installing, updating and uninstalling never touch them.

**Uninstall:** use *Settings > Apps*, or run `Rhea.exe --uninstall`. It removes the shortcut, the registry entry and the install folder. `~/.rhea` stays; delete it yourself to remove everything.

**Updates.** Rhea can look up the newest GitHub release (`installer::check_for_update`), download the installer for your platform and run it, which replaces the installed copy as above. On macOS the download is the `.dmg`, opened for you to drag into Applications.

The repository the releases come from is the constant `REPO` in `src/installer.rs`.
