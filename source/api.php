<?php
// Endpoint for the GPUTuner page: POST op=state | op=apply&req=<json>. Shells out to api.py (root).
header('Content-Type: application/json');
$var = @parse_ini_file('/var/local/emhttp/var.ini') ?: [];
if ($_SERVER['REQUEST_METHOD'] !== 'POST' || !hash_equals((string)($var['csrf_token'] ?? ''), (string)($_POST['csrf_token'] ?? ''))) {
  http_response_code(403); echo '{"ok":false,"error":"bad csrf token"}'; exit;
}
$op = $_POST['op'] ?? '';
$stdin = '';
if ($op === 'apply') {
  $req = json_decode($_POST['req'] ?? '', true);
  if (!is_array($req) || !in_array($req['op'] ?? '', ['set_power','set_fan','set_clock_cap','baseline','set_budget'], true)) {
    echo '{"ok":false,"error":"bad request"}'; exit;
  }
  $stdin = json_encode($req);
} elseif ($op !== 'state') {
  echo '{"ok":false,"error":"bad op"}'; exit;
}
$p = proc_open(['python3', '/usr/local/emhttp/plugins/GPUTuner/api.py', $op], [0=>['pipe','r'],1=>['pipe','w'],2=>['pipe','w']], $pipes);
if (!is_resource($p)) { echo '{"ok":false,"error":"cannot start python3"}'; exit; }
fwrite($pipes[0], $stdin); fclose($pipes[0]);
$out = stream_get_contents($pipes[1]); fclose($pipes[1]); fclose($pipes[2]); proc_close($p);
echo $out !== '' ? $out : '{"ok":false,"error":"no output from api.py"}';
