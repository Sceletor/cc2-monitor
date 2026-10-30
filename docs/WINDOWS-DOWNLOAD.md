# CC2 Monitor — Windows x64

This is a native, unsigned Windows x64 build from Sceletor/cc2-monitor.

## Published beta

1. Open https://github.com/Sceletor/cc2-monitor/releases/tag/v1.0.1-beta.1.
2. Under **Assets**, download `cc2-monitor-windows-x64.zip`.
3. Extract the ZIP and run `cc2-monitor.exe`. No Rust installation is needed.
   The ZIP also contains this README, `LICENSE`, and `SHA256SUMS.txt`.

The release also provides `SHA256SUMS.txt` for the EXE and
`cc2-monitor-windows-x64.zip.sha256` for the ZIP.

## Development builds from Actions

Sign in to GitHub and open
https://github.com/Sceletor/cc2-monitor/actions/workflows/security-regression.yml.
Select a successful run for the intended branch (`master` for the merged version)
and check its commit. Under **Artifacts**, download **cc2-monitor-windows-x64**,
then extract the ZIP and run `cc2-monitor.exe`.

Artifacts expire after 14 days. A new PR update rebuilds the app. A maintainer
can also use **Run workflow**, selecting the intended branch.
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
