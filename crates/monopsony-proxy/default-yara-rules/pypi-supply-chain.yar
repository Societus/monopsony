// Monopsony default YARA rules for PyPI supply-chain threat detection.

rule pypi_setup_py_obfuscation {
    meta:
        description = "Obfuscated code patterns in setup.py"
        severity = "medium"
        confidence = "low"
    strings:
        $exec = "exec(" ascii
        $eval = "eval(" ascii
        $compile = "compile(" ascii
        $base64 = "base64.b64decode" ascii
        $marshal = "marshal.loads" ascii
        $zlib = "zlib.decompress" ascii
        $subprocess = "subprocess" ascii
    condition:
        2 of ($exec, $eval, $compile, $base64, $marshal, $zlib) and $subprocess
}

rule pypi_suspicious_network {
    meta:
        description = "Suspicious network activity from Python packages"
        severity = "high"
        confidence = "medium"
    strings:
        $urllib = "urllib.request" ascii
        $requests = "requests.get" ascii
        $requests_post = "requests.post" ascii
        $socket = "socket.socket" ascii
        $subprocess = "subprocess.Popen" ascii
        $os_system = "os.system(" ascii
    condition:
        ($urllib or $requests or $requests_post or $socket) and ($subprocess or $os_system)
}

rule pypi_sensitive_file_access {
    meta:
        description = "Access to sensitive files from Python packages"
        severity = "high"
        confidence = "low"
    strings:
        $etc_shadow = "/etc/shadow" ascii
        $ssh_key = "id_rsa" ascii
        $aws_creds = ".aws/credentials" ascii
        $env = "/.env" ascii
        $gcp = ".config/gcloud" ascii
        $os_environ = "os.environ" ascii
    condition:
        2 of ($etc_shadow, $ssh_key, $aws_creds, $env, $gcp) and $os_environ
}

rule pypi_cryptominer_indicator {
    meta:
        description = "Cryptocurrency mining indicators in Python packages"
        severity = "critical"
        confidence = "medium"
    strings:
        $stratum = "stratum+tcp" ascii
        $xmr = "monero" ascii nocase
        $coinhive = "coinhive" ascii nocase
        $hashrate = "hashrate" ascii
        $ctypes = "ctypes.CDLL" ascii
        $mmap = "mmap.mmap" ascii
    condition:
        2 of ($stratum, $xmr, $coinhive, $hashrate) or ($ctypes and 1 of ($stratum, $xmr, $hashrate))
}

rule pypi_data_exfiltration {
    meta:
        description = "Data exfiltration patterns in Python packages"
        severity = "high"
        confidence = "medium"
    strings:
        $walk = "os.walk" ascii
        $glob = "glob.glob" ascii
        $read = ".read()" ascii
        $post = "requests.post" ascii
        $urlopen = "urlopen" ascii
        $home = "os.path.expanduser" ascii
    condition:
        ($walk or $glob) and ($post or $urlopen) and $home
}
