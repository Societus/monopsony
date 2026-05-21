// Monopsony default YARA rules for npm supply-chain threat detection.
// These are conservative heuristics — not exploits detection.
// False positives are expected; tune entropy thresholds in config.toml.

rule npm_install_script_obfuscation {
    meta:
        description = "Detects obfuscated code patterns in npm install scripts"
        severity = "medium"
        confidence = "low"
    strings:
        $s1 = "eval(atob(" ascii
        $s2 = "Function(" ascii
        $s3 = "\\x" fullword
        $s4 = "require('child_process')" ascii
    condition:
        2 of ($s1, $s2, $s3) and $s4
}

rule npm_suspicious_preinstall {
    meta:
        description = "Suspicious commands in preinstall/postinstall scripts"
        severity = "high"
        confidence = "medium"
    strings:
        $curl = "curl " ascii
        $wget = "wget " ascii
        $pipe = "| sh" ascii
        $pipe2 = "| bash" ascii
        $base64 = "base64 --decode" ascii
        $certutil = "certutil" ascii
    condition:
        any of them
}

rule npm_sensitive_file_access {
    meta:
        description = "Access to sensitive files from npm package"
        severity = "high"
        confidence = "low"
    strings:
        $etc_shadow = "/etc/shadow" ascii
        $etc_passwd = "/etc/passwd" ascii
        $ssh_key = ".ssh/id_rsa" ascii
        $ssh_key2 = ".ssh/id_ed25519" ascii
        $aws_creds = ".aws/credentials" ascii
        $env = "/.env" ascii
        $gcp_creds = ".config/gcloud" ascii
    condition:
        2 of them
}

rule npm_cryptominer_indicator {
    meta:
        description = "Indicators of cryptocurrency mining in npm packages"
        severity = "critical"
        confidence = "medium"
    strings:
        $stratum = "stratum+tcp" ascii
        $stratum2 = "stratum://" ascii
        $pool = "mining-pool" ascii
        $xmr = "monero" ascii nocase
        $coinhive = "coinhive" ascii nocase
        $hashrate = "hashrate" ascii
        $webassembly = "WebAssembly.instantiate" ascii
    condition:
        2 of them
}

rule npm_dns_tunneling {
    meta:
        description = "DNS exfiltration patterns in npm packages"
        severity = "high"
        confidence = "medium"
    strings:
        $dgram = "require('dgram')" ascii
        $dns = "require('dns')" ascii
        $lookup = ".lookup(" ascii
        $b64 = "Buffer.from(" ascii
        $reverse = ".reverse(" ascii
    condition:
        ($dgram or $dns) and ($b64 or $reverse)
}
