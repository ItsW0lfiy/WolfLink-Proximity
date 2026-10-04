param(
    [Parameter(Mandatory)]
    [string]$Address,

    [int]$Radius = 512
)

$ErrorActionPreference = 'Stop'

$proc = Get-Process -Name RobloxPlayerBeta -ErrorAction Stop |
    Sort-Object StartTime -Descending |
    Select-Object -First 1

if (-not ("HitInspector" -as [type])) {
    Add-Type @"
using System;
using System.Runtime.InteropServices;

public static class HitInspector
{
    [DllImport("kernel32.dll", SetLastError = true)]
    public static extern IntPtr OpenProcess(
        uint dwDesiredAccess,
        bool bInheritHandle,
        uint dwProcessId
    );

    [DllImport("kernel32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static extern bool ReadProcessMemory(
        IntPtr hProcess,
        IntPtr lpBaseAddress,
        byte[] lpBuffer,
        UIntPtr nSize,
        out UIntPtr lpNumberOfBytesRead
    );

    [DllImport("kernel32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static extern bool CloseHandle(IntPtr hObject);
}
"@
}

$hex = $Address.Trim()

if ($hex.StartsWith("0x")) {
    $hex = $hex.Substring(2)
}

$target = [Convert]::ToInt64($hex, 16)
$start  = $target - $Radius
$length = ($Radius * 2) + 1

$PROCESS_VM_READ            = 0x0010
$PROCESS_QUERY_INFORMATION  = 0x0400

$handle = [HitInspector]::OpenProcess(
    $PROCESS_VM_READ -bor $PROCESS_QUERY_INFORMATION,
    $false,
    [uint32]$proc.Id
)

if ($handle -eq [IntPtr]::Zero) {
    $err = [Runtime.InteropServices.Marshal]::GetLastWin32Error()
    throw "OpenProcess failed: Win32 error $err"
}

try {
    $buffer = New-Object byte[] $length
    $read   = [UIntPtr]::Zero

    $ok = [HitInspector]::ReadProcessMemory(
        $handle,
        [IntPtr]$start,
        $buffer,
        [UIntPtr]$length,
        [ref]$read
    )

    if (-not $ok) {
        $err = [Runtime.InteropServices.Marshal]::GetLastWin32Error()
        throw "ReadProcessMemory failed: Win32 error $err"
    }

    $count = [int]$read.ToUInt64()

    Write-Host ""
    Write-Host ("PID       : {0}" -f $proc.Id)
    Write-Host ("Hit       : 0x{0:X16}" -f $target)
    Write-Host ("Read from : 0x{0:X16}" -f $start)
    Write-Host ("Bytes     : {0}" -f $count)
    Write-Host ""

    #
    # Hex + ASCII dump
    #

    for ($offset = 0; $offset -lt $count; $offset += 16) {

        $lineLength = [Math]::Min(16, $count - $offset)

        $hexBytes = @()
        $chars    = @()

        for ($i = 0; $i -lt $lineLength; $i++) {

            $b = $buffer[$offset + $i]

            $hexBytes += "{0:X2}" -f $b

            if ($b -ge 0x20 -and $b -le 0x7E) {
                $chars += [char]$b
            }
            else {
                $chars += '.'
            }
        }

        $addr = $start + $offset

        $hexText = ($hexBytes -join ' ').PadRight(47)
        $ascii   = $chars -join ''

        $marker = " "

        if ($addr -le $target -and ($addr + 15) -ge $target) {
            $marker = ">"
        }

        Write-Host (
            "{0} 0x{1:X16}  {2}  |{3}|" -f `
                $marker,
                $addr,
                $hexText,
                $ascii
        )
    }

    Write-Host ""
    Write-Host "---- Printable ASCII strings ----"

    $asciiText = [Text.Encoding]::ASCII.GetString(
        $buffer,
        0,
        $count
    )

    [regex]::Matches(
        $asciiText,
        '[\x20-\x7E]{4,}'
    ) |
    ForEach-Object {

        $absolute = $start + $_.Index

        Write-Host (
            "0x{0:X16}  {1}" -f `
                $absolute,
                $_.Value
        )
    }

    Write-Host ""
    Write-Host "---- UTF-16LE-like strings ----"

    for ($alignment = 0; $alignment -le 1; $alignment++) {

        if (($count - $alignment) -lt 4) {
            continue
        }

        $usable = $count - $alignment

        if (($usable % 2) -ne 0) {
            $usable--
        }

        $unicode = [Text.Encoding]::Unicode.GetString(
            $buffer,
            $alignment,
            $usable
        )

        [regex]::Matches(
            $unicode,
            '[\x20-\x7E]{4,}'
        ) |
        ForEach-Object {

            $absolute = $start + $alignment + ($_.Index * 2)

            Write-Host (
                "0x{0:X16}  {1}" -f `
                    $absolute,
                    $_.Value
            )
        }
    }
}
finally {
    [void][HitInspector]::CloseHandle($handle)
}