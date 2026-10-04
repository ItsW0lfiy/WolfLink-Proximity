param(
    [Parameter(Mandatory)]
    [string]$Needle,

    [int]$MaxHits = 50
)

$ErrorActionPreference = 'Stop'

$proc = Get-Process -Name RobloxPlayerBeta -ErrorAction Stop |
    Sort-Object StartTime -Descending |
    Select-Object -First 1

Add-Type @"
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;

public static class MemScan
{
    [StructLayout(LayoutKind.Sequential)]
    public struct MEMORY_BASIC_INFORMATION
    {
        public IntPtr BaseAddress;
        public IntPtr AllocationBase;
        public uint AllocationProtect;
        public UIntPtr RegionSize;
        public uint State;
        public uint Protect;
        public uint Type;
    }

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

    [DllImport("kernel32.dll", SetLastError = true)]
    public static extern UIntPtr VirtualQueryEx(
        IntPtr hProcess,
        IntPtr lpAddress,
        out MEMORY_BASIC_INFORMATION lpBuffer,
        UIntPtr dwLength
    );

    [DllImport("kernel32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static extern bool CloseHandle(IntPtr hObject);

    public static int[] FindBytes(byte[] data, int length, byte[] pattern)
    {
        var results = new List<int>();

        if (pattern == null || pattern.Length == 0)
            return results.ToArray();

        int limit = length - pattern.Length;

        for (int i = 0; i <= limit; i++)
        {
            bool match = true;

            for (int j = 0; j < pattern.Length; j++)
            {
                if (data[i + j] != pattern[j])
                {
                    match = false;
                    break;
                }
            }

            if (match)
                results.Add(i);
        }

        return results.ToArray();
    }
}
"@

# Query information + read memory. No VM_WRITE / VM_OPERATION.
$PROCESS_VM_READ = 0x0010
$PROCESS_QUERY_INFORMATION = 0x0400

$handle = [MemScan]::OpenProcess(
    $PROCESS_VM_READ -bor $PROCESS_QUERY_INFORMATION,
    $false,
    [uint32]$proc.Id
)

if ($handle -eq [IntPtr]::Zero) {
    $err = [Runtime.InteropServices.Marshal]::GetLastWin32Error()
    throw "OpenProcess failed: Win32 error $err"
}

$utf8  = [Text.Encoding]::UTF8.GetBytes($Needle)
$utf16 = [Text.Encoding]::Unicode.GetBytes($Needle)

$patterns = @(
    @{
        Name = "UTF-8"
        Bytes = $utf8
    },
    @{
        Name = "UTF-16LE"
        Bytes = $utf16
    }
)

$mbi = [MemScan+MEMORY_BASIC_INFORMATION]::new()
$mbiSize = [Runtime.InteropServices.Marshal]::SizeOf(
    [type][MemScan+MEMORY_BASIC_INFORMATION]
)

$MEM_COMMIT  = 0x1000
$MEM_PRIVATE = 0x20000
$MEM_MAPPED  = 0x40000

$PAGE_READONLY  = 0x02
$PAGE_READWRITE = 0x04
$PAGE_WRITECOPY = 0x08
$PAGE_GUARD     = 0x100

# 1 MiB at a time so we don't allocate enormous buffers.
$chunkSize = 1MB

$hits = 0
$regionsScanned = 0
$bytesScanned = [uint64]0

try {
    Write-Host ""
    Write-Host "Process : $($proc.ProcessName)"
    Write-Host "PID     : $($proc.Id)"
    Write-Host "Needle  : '$Needle'"
    Write-Host ""

    $address = [long]0

    while ($hits -lt $MaxHits) {

        $q = [MemScan]::VirtualQueryEx(
            $handle,
            [IntPtr]$address,
            [ref]$mbi,
            [UIntPtr]$mbiSize
        )

        if ($q -eq [UIntPtr]::Zero) {
            break
        }

        $base = $mbi.BaseAddress.ToInt64()
        $size = [long]$mbi.RegionSize.ToUInt64()

        if ($size -le 0) {
            break
        }

        $basicProtect = $mbi.Protect -band 0xFF

        $ordinaryDataPage =
            $mbi.State -eq $MEM_COMMIT -and
            ($mbi.Protect -band $PAGE_GUARD) -eq 0 -and
            $basicProtect -in @(
                $PAGE_READONLY,
                $PAGE_READWRITE,
                $PAGE_WRITECOPY
            ) -and
            $mbi.Type -in @(
                $MEM_PRIVATE,
                $MEM_MAPPED
            )

        if ($ordinaryDataPage) {

            $regionsScanned++

            foreach ($pattern in $patterns) {

                $patternBytes = $pattern.Bytes

                # Retain enough overlap so strings crossing chunk boundaries
                # aren't missed.
                $overlap = [Math]::Max(0, $patternBytes.Length - 1)

                $offset = [long]0

                while ($offset -lt $size -and $hits -lt $MaxHits) {

                    $remaining = $size - $offset
                    $readSize = [int][Math]::Min(
                        [long]$chunkSize,
                        $remaining
                    )

                    if ($readSize -le 0) {
                        break
                    }

                    $buffer = New-Object byte[] $readSize
                    $read = [UIntPtr]::Zero

                    $readAddress = $base + $offset

                    $ok = [MemScan]::ReadProcessMemory(
                        $handle,
                        [IntPtr]$readAddress,
                        $buffer,
                        [UIntPtr]$readSize,
                        [ref]$read
                    )

                    if ($ok -and $read.ToUInt64() -gt 0) {

                        $actual = [int]$read.ToUInt64()
                        $bytesScanned += [uint64]$actual

                        $matches = [MemScan]::FindBytes(
                            $buffer,
                            $actual,
                            $patternBytes
                        )

                        foreach ($m in $matches) {

                            $hitAddress = $readAddress + $m

                            $message = (
    "[{0}] 0x{1:X16}  region=0x{2:X16}  size={3:N0}  protect=0x{4:X}  type=0x{5:X}" -f `
        $pattern.Name,
        $hitAddress,
        $base,
        $size,
        $mbi.Protect,
        $mbi.Type
)
Write-Host $message

                            $hits++

                            if ($hits -ge $MaxHits) {
                                break
                            }
                        }
                    }

                    # Move forward with a small overlap.
                    if ($readSize -le $overlap) {
                        break
                    }

                    $offset += ($readSize - $overlap)
                }
            }
        }

        $next = $base + $size

        if ($next -le $address) {
            break
        }

        $address = $next
    }

    Write-Host ""
    Write-Host "Done."
    Write-Host "Regions scanned : $regionsScanned"
    Write-Host ("Bytes scanned   : {0:N0}" -f $bytesScanned)
    Write-Host "Hits found      : $hits"
}
finally {
    [void][MemScan]::CloseHandle($handle)
}

