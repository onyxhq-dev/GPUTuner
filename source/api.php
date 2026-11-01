<?php
header('Content-Type: application/json');
if ($_SERVER['REQUEST_METHOD'] !== 'POST') {
  http_response_code(405); echo '{"ok":false,"error":"POST only"}'; exit;
}
$op = $_POST['op'] ?? '';
if ($op === 'live') {
  $f = '/run/gpu-tuner/live.json';
  clearstatcache();
  echo (is_file($f) && time() - filemtime($f) < 10) ? '{"ok":true,' . substr(file_get_contents($f), 1) : '{"ok":false,"error":"no live data yet"}';
  exit;
}
if ($op === 'history') {
  $to = isset($_POST['to']) ? (int)$_POST['to'] : time();
  $from = isset($_POST['from']) ? (int)$_POST['from'] : $to - max(60, (int)($_POST['window'] ?? 900));
  $from = max($from, $to - 31 * 86400);
  if ($from >= $to) { echo '{"ok":false,"error":"bad range"}'; exit; }
  $ram = [];
  foreach (@file('/run/gpu-tuner/history.jsonl', FILE_IGNORE_NEW_LINES) ?: [] as $line) {
    $r = json_decode($line, true);
    if (is_array($r)) $ram[] = $r;
  }
  $firstRam = $ram ? $ram[0]['t'] : PHP_INT_MAX;
  $rows = [];
  if ($from < $firstRam) {
    for ($d = (int)floor($from / 86400) * 86400; $d <= $to; $d += 86400) {
      $f = '/boot/config/plugins/gputuner/data/history/' . gmdate('Y-m-d', $d) . '.jsonl';
      foreach (is_file($f) ? file($f, FILE_IGNORE_NEW_LINES) : [] as $line) {
        $r = json_decode($line, true);
        if (is_array($r) && $r['t'] >= $from && $r['t'] <= $to && $r['t'] < $firstRam) $rows[] = $r;
      }
    }
  }
  foreach ($ram as $r) if ($r['t'] >= $from && $r['t'] <= $to) $rows[] = $r;
  $n = max(1, min(300, (int)floor(($to - $from) / 5)));
  $w = ($to - $from) / $n;
  $names = ['temp', 'fan', 'power', 'limit', 'clock', 'util', 'vram'];
  $uuids = $rows ? array_keys(end($rows)['g']) : [];
  $acc = [];
  foreach ($rows as $r) {
    $i = min($n - 1, (int)floor(($r['t'] - $from) / $w));
    foreach ($uuids as $u) {
      $v = $r['g'][$u] ?? null;
      if (!$v) continue;
      $v[6] = !empty($v[7]) ? $v[6] / $v[7] * 100 : null;
      foreach ($names as $k => $name) {
        if ($v[$k] === null) continue;
        $acc[$u][$name][$i][0] = ($acc[$u][$name][$i][0] ?? 0) + $v[$k];
        $acc[$u][$name][$i][1] = ($acc[$u][$name][$i][1] ?? 0) + 1;
      }
    }
  }
  $out = ['ok' => true, 't' => [], 'gpus' => []];
  for ($i = 0; $i < $n; $i++) $out['t'][] = (int)round($from + ($i + 0.5) * $w);
  foreach ($uuids as $u) foreach ($names as $name) for ($i = 0; $i < $n; $i++) {
    $c = $acc[$u][$name][$i] ?? null;
    $out['gpus'][$u][$name][] = $c ? round($c[0] / $c[1], 1) : null;
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
