// Monopsony default YARA rules for generic supply-chain threat detection.
// Applies to any package ecosystem.

rule generic_embedded_executable {
    meta:
        description = "Detects embedded ELF/MachO/PE binaries in package archives"
        severity = "critical"
        confidence = "high"
    strings:
        $elf = { 7f 45 4c 46 }       // ELF magic
        $macho = { ce fa ed fe }      // Mach-O magic
        $macho64 = { cf fa ed fe }    // Mach-O 64-bit magic
        $pe = { 4d 5a }              // PE/DOS magic (MZ)
    condition:
        any of them
}

rule generic_large_binary_blob {
    meta:
        description = "Large binary blobs that may contain hidden payloads"
        severity = "medium"
        confidence = "low"
    strings:
        // High-entropy blocks (likely encrypted/compressed data)
        $blob = { 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 }
    condition:
        #blob > 50
}

rule generic_reverse_shell {
    meta:
        description = "Reverse shell patterns in any package type"
        severity = "critical"
        confidence = "high"
    strings:
        $bash_tcp = "/dev/tcp/" ascii
        $bash_udp = "/dev/udp/" ascii
        $nc_pipe = "|/bin/sh" ascii
        $nc_pipe2 = "| /bin/sh" ascii
        $socat = "socat exec" ascii
        $python_socket = "socket.socket(socket.AF_INET, socket.SOCK_STREAM)" ascii
        $pwsh_download = "IEX (New-Object Net.WebClient)" ascii
        $pwsh_download2 = "Invoke-WebRequest" ascii
    condition:
        any of them
}

rule generic_c2_beacon {
    meta:
        description = "Command-and-control beacon patterns"
        severity = "critical"
        confidence = "medium"
    strings:
        $interval = "setInterval" ascii
        $settimeout = "setTimeout" ascii
        $fetch = "fetch(" ascii
        $xhr = "XMLHttpRequest" ascii
        $beacon = "navigator.sendBeacon" ascii
        $ws = "new WebSocket(" ascii
    condition:
        ($interval or $settimeout) and ($fetch or $xhr or $beacon or $ws)
}
