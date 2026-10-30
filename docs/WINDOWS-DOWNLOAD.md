# CC2 Monitor — Windows x64

This is a native, unsigned Windows x64 build from Sceletor/cc2-monitor.

1. Sign in to GitHub and open https://github.com/Sceletor/cc2-monitor/actions/workflows/security-regression.yml.
2. Select a successful run for `fix/security-canvas-safety-20261030` (PR #1).
   Check the commit shown in the run before downloading.
3. Under **Artifacts**, download **cc2-monitor-windows-x64**. GitHub provides
   a ZIP containing `cc2-monitor.exe`, this README, `LICENSE`, and `SHA256SUMS.txt`.
4. Extract the ZIP and run `cc2-monitor.exe`. No Rust installation is needed.

Artifacts expire after 14 days. A new PR update rebuilds the app. A maintainer
can also use **Run workflow** if available, selecting the same branch.
This workflow does not publish a release or merge the PR.

To verify the EXE in PowerShell from the extracted folder:

```powershell
$expected = (Get-Content .\SHA256SUMS.txt).Split(' ')[0]
$actual = (Get-FileHash .\cc2-monitor.exe -Algorithm SHA256).Hash
if ($actual -ine $expected) { throw 'SHA-256 mismatch' }
Write-Host 'SHA-256 OK'
```

The EXE hash also appears in the Actions run summary. A checksum detects
file corruption; it is not a publisher signature. Windows may display an
unknown-publisher warning because this executable is not code-signed.
Settings are stored in `%APPDATA%\cc2-monitor`.
