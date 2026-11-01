<?php
header('Content-Type: application/json');
if ($_SERVER['REQUEST_METHOD'] !== 'POST') {
  http_response_code(405); echo '{"ok":false,"error":"POST only"}'; exit;
}
$op = $_POST['op'] ?? '';
if ($op === 'history') {
  $cut = time() - max(60, min(21600, (int)($_POST['window'] ?? 900)));
  $rows = [];
  foreach (@file('/run/gpu-tuner/history.jsonl', FILE_IGNORE_NEW_LINES) ?: [] as $line) {
    $r = json_decode($line, true);
    if (is_array($r) && $r['t'] >= $cut) $rows[] = $r;
  }
  $out = ['ok' => true, 't' => [], 'gpus' => []];
  if ($rows) {
    $uuids = array_keys(end($rows)['g']);
    $step = max(1, (int)ceil(count($rows) / 300));
    $names = ['temp', 'fan', 'power', 'limit', 'clock', 'util'];
    for ($i = 0; $i < count($rows); $i += $step) {
      $out['t'][] = $rows[$i]['t'];
      foreach ($uuids as $u) {
        $v = $rows[$i]['g'][$u] ?? array_fill(0, 8, null);
        foreach ($names as $k => $name) $out['gpus'][$u][$name][] = $v[$k];
        $out['gpus'][$u]['vram'][] = !empty($v[7]) ? round($v[6] / $v[7] * 100, 1) : null;
      }
    }
  }
  echo json_encode($out); exit;
}
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
$p = proc_open(['/usr/local/lib/gputuner/python/bin/python3', '/usr/local/emhttp/plugins/GPUTuner/api.py', $op], [0=>['pipe','r'],1=>['pipe','w'],2=>['pipe','w']], $pipes);
if (!is_resource($p)) { echo '{"ok":false,"error":"cannot start python3"}'; exit; }
fwrite($pipes[0], $stdin); fclose($pipes[0]);
$out = stream_get_contents($pipes[1]); fclose($pipes[1]); fclose($pipes[2]); proc_close($p);
echo $out !== '' ? $out : '{"ok":false,"error":"no output from api.py"}';
