$ErrorActionPreference = 'Stop'

$proc = Get-Process -Name RobloxPlayerBeta -ErrorAction Stop |
    Sort-Object StartTime -Descending |
    Select-Object -First 1

Add-Type @"
using System;
using System.Runtime.InteropServices;

public static class RobloxMemoryReader
{
    [StructLayout(LayoutKind.Sequential)]
    public struct MEMORY_BASIC_INFORMATION
    {
        public IntPtr  BaseAddress;
        public IntPtr  AllocationBase;
        public uint    AllocationProtect;
        public UIntPtr RegionSize;
        public uint    State;
        public uint    Protect;
        public uint    Type;
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

    [DllImport("kernel32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static extern bool CloseHandle(IntPtr hObject);
}
"@

# PROCESS_VM_READ             = 0x0010
# PROCESS_QUERY_INFORMATION  = 0x0400
$access = 0x0010 -bor 0x0400

$handle = [RobloxMemoryReader]::OpenProcess(
    $access,
    $false,
    [uint32]$proc.Id
)

if ($handle -eq [IntPtr]::Zero) {
    $err = [Runtime.InteropServices.Marshal]::GetLastWin32Error()
    throw "OpenProcess failed. Win32 error: $err"
}

try {
    Write-Host ""
    Write-Host "Roblox PID : $($proc.Id)"
    Write-Host "Started    : $($proc.StartTime)"
    Write-Host "Handle     : 0x$('{0:X}' -f $handle.ToInt64())"
    Write-Host ""

    #
    # Test 1: main executable image
    #

    try {
        $base = $proc.MainModule.BaseAddress
        Write-Host "Main module base: 0x$('{0:X16}' -f $base.ToInt64())"

        $buffer = New-Object byte[] 64
        $bytesRead = [UIntPtr]::Zero

        $ok = [RobloxMemoryReader]::ReadProcessMemory(
            $handle,
            $base,
            $buffer,
            [UIntPtr]64,
            [ref]$bytesRead
        )

        if ($ok) {
            $n = [int]$bytesRead.ToUInt64()

            Write-Host "Read $n bytes."
            Write-Host ([BitConverter]::ToString($buffer, 0, $n))

            if (
                $n -ge 2 -and
                $buffer[0] -eq 0x4D -and
                $buffer[1] -eq 0x5A
            ) {
                Write-Host ""
                Write-Host "[SUCCESS] Found MZ header. ReadProcessMemory works."
            }
        }
        else {
            $err = [Runtime.InteropServices.Marshal]::GetLastWin32Error()
            Write-Host "[FAILED] Main-module read. Win32 error: $err"
        }
    }
    catch {
        Write-Host "[FAILED] Couldn't obtain main-module base:"
        Write-Host $_.Exception.Message
        $base = [IntPtr]::Zero
    }

    Write-Host ""
    Write-Host "---- Sampling readable memory regions ----"

    #
    # Test 2: inspect some virtual memory regions
    #

    $mbi = [RobloxMemoryReader+MEMORY_BASIC_INFORMATION]::new()
    $mbiSize = [Runtime.InteropServices.Marshal]::SizeOf(
        [type][RobloxMemoryReader+MEMORY_BASIC_INFORMATION]
    )

    if ($base -ne [IntPtr]::Zero) {
        $address = $base.ToInt64()
    }
    else {
        $address = 0
    }

    $found = 0
    $queries = 0

    while ($found -lt 12 -and $queries -lt 5000) {
        $queries++

        $result = [RobloxMemoryReader]::VirtualQueryEx(
            $handle,
            [IntPtr]$address,
            [ref]$mbi,
            [UIntPtr]$mbiSize
        )

        if ($result -eq [UIntPtr]::Zero) {
            break
        }

        $regionBase = $mbi.BaseAddress.ToInt64()
        $regionSize = [long]$mbi.RegionSize.ToUInt64()

        # MEM_COMMIT
        $committed = $mbi.State -eq 0x1000

        # Skip PAGE_GUARD and PAGE_NOACCESS
        $guarded  = ($mbi.Protect -band 0x100) -ne 0
        $noAccess = ($mbi.Protect -band 0xFF) -eq 0x01

        $basicProtection = $mbi.Protect -band 0xFF

        $readableProtections = @(
            0x02, # PAGE_READONLY
            0x04, # PAGE_READWRITE
            0x08, # PAGE_WRITECOPY
            0x20, # PAGE_EXECUTE_READ
            0x40, # PAGE_EXECUTE_READWRITE
            0x80  # PAGE_EXECUTE_WRITECOPY
        )

        $readable =
            $committed -and
            -not $guarded -and
            -not $noAccess -and
            ($basicProtection -in $readableProtections)

        if ($readable -and $regionSize -gt 0) {
            $sampleSize = [int][Math]::Min(64, $regionSize)
            $sample = New-Object byte[] $sampleSize
            $sampleRead = [UIntPtr]::Zero

            $ok = [RobloxMemoryReader]::ReadProcessMemory(
                $handle,
                $mbi.BaseAddress,
                $sample,
                [UIntPtr]$sampleSize,
                [ref]$sampleRead
            )

            if ($ok -and $sampleRead.ToUInt64() -gt 0) {
                $n = [int]$sampleRead.ToUInt64()

                Write-Host ""
                Write-Host ("Region #{0}" -f ($found + 1))
                Write-Host (" Base    : 0x{0:X16}" -f $regionBase)
                Write-Host (" Size    : {0:N0} bytes" -f $regionSize)
                Write-Host (" Protect : 0x{0:X}" -f $mbi.Protect)
                Write-Host (" Data    : {0}" -f (
                    [BitConverter]::ToString($sample, 0, $n)
                ))

                $found++
            }
        }

        if ($regionSize -le 0) {
            break
        }

        $next = $regionBase + $regionSize

        if ($next -le $address) {
            break
        }

        $address = $next
    }

    Write-Host ""
    Write-Host "Readable sampled regions: $found"
}
finally {
    [void][RobloxMemoryReader]::CloseHandle($handle)
}