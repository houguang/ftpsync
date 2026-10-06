/* ============================================================
   FTPSync 前端逻辑
   与 Rust 后端通过 Tauri IPC 通信（withGlobalTauri 全局注入）。
   ============================================================ */

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (id) => document.getElementById(id);

const MAX_LOG_LINES = 1200;
let logCount = 0;
let running = false;
let dirty = false;

/* ------------------------------------------------------------ 表单读写 */

/** 从界面读取完整配置对象。 */
function readForm() {
  return {
    localDir: $('localDir').value.trim(),
    remoteDir: $('remoteDir').value.trim(),
    mirrorDelete: $('mirrorDelete').checked,
    cleanExtraneous: $('cleanExtraneous').checked,
    ignorePatterns: $('ignorePatterns')
      .value.split('\n')
      .map((s) => s.trim())
      .filter(Boolean),
    autoStart: $('autoStart').checked,
    syncOnStart: $('syncOnStart').checked,
    debounceMs: Number($('debounceMs').value) || 800,
    ftp: {
      host: $('ftpHost').value.trim(),
      port: Number($('ftpPort').value) || 21,
      username: $('ftpUser').value,
      password: $('ftpPass').value,
      secure: $('ftpSecure').checked,
      timeoutSecs: Number($('ftpTimeout').value) || 30,
    },
  };
}

/** 把配置对象写回界面。 */
function writeForm(cfg) {
  $('localDir').value = cfg.localDir || '';
  $('remoteDir').value = cfg.remoteDir || '/';
  $('mirrorDelete').checked = !!cfg.mirrorDelete;
  $('cleanExtraneous').checked = !!cfg.cleanExtraneous;
  $('ignorePatterns').value = (cfg.ignorePatterns || []).join('\n');
  $('autoStart').checked = !!cfg.autoStart;
  $('syncOnStart').checked = !!cfg.syncOnStart;
  $('debounceMs').value = cfg.debounceMs || 800;

  const ftp = cfg.ftp || {};
  $('ftpHost').value = ftp.host || '';
  $('ftpPort').value = ftp.port || 21;
  $('ftpUser').value = ftp.username || '';
  $('ftpPass').value = ftp.password || '';
  $('ftpSecure').checked = !!ftp.secure;
  $('ftpTimeout').value = ftp.timeoutSecs || 30;

  setDirty(false);
}

function setDirty(value) {
  dirty = value;
  $('dirtyFlag').hidden = !value;
}

/* ------------------------------------------------------------ 交互反馈 */

let toastTimer = null;
function toast(message, kind = '') {
  const el = $('toast');
  el.textContent = message;
  el.className = 'toast' + (kind ? ' ' + kind : '');
  el.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => {
    el.hidden = true;
  }, kind === 'err' ? 6000 : 3000);
}

function setTestResult(text, cls) {
  const el = $('testResult');
  el.textContent = text;
  el.className = 'inline-result' + (cls ? ' ' + cls : '');
}

/* ------------------------------------------------------------ 日志 */

function fmtTime(ts) {
  const d = new Date(ts);
  const p = (n) => String(n).padStart(2, '0');
  return `${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`;
}

function appendLog(entry) {
  const box = $('log');
  const empty = box.querySelector('.log-empty');
  if (empty) empty.remove();

  const line = document.createElement('div');
  line.className = 'log-line ' + (entry.level || 'info');

  const time = document.createElement('span');
  time.className = 'log-time';
  time.textContent = fmtTime(entry.ts);

  const msg = document.createElement('span');
  msg.className = 'log-msg';
  msg.textContent = entry.message;

  line.append(time, msg);
  box.appendChild(line);
  logCount += 1;

  while (logCount > MAX_LOG_LINES && box.firstElementChild) {
    box.removeChild(box.firstElementChild);
    logCount -= 1;
  }

  if ($('autoScroll').checked) box.scrollTop = box.scrollHeight;
}

function renderLogs(entries) {
  const box = $('log');
  box.innerHTML = '';
  logCount = 0;
  if (!entries.length) {
    const p = document.createElement('div');
    p.className = 'log-empty';
    p.textContent = '暂无日志。启动监听后，文件同步记录会显示在这里。';
    box.appendChild(p);
    return;
  }
  entries.forEach(appendLog);
  box.scrollTop = box.scrollHeight;
}

/* ------------------------------------------------------------ 状态渲染 */

const PHASE_TEXT = {
  idle: '监听中',
  connecting: '连接服务器',
  scanning: '扫描本地文件',
  syncing: '同步中',
  stopped: '已停止',
};

function applyStatus(stats) {
  running = !!stats.running;

  $('stUploaded').textContent = stats.uploaded;
  $('stDeleted').textContent = stats.deleted;
  $('stSkipped').textContent = stats.skipped;
  $('stFailed').textContent = stats.failed;
  $('stPending').textContent = stats.pending;
  $('stTotal').textContent = stats.totalFiles;

  const pill = $('statusPill');
  let state = 'stopped';
  let text = '已停止';
  if (stats.running) {
    if (stats.phase === 'syncing' || stats.phase === 'scanning') {
      state = 'busy';
      text = PHASE_TEXT[stats.phase] || '同步中';
    } else if (stats.failed > 0 && stats.lastError) {
      state = 'error';
      text = '监听中（有错误）';
    } else {
      state = 'running';
      text = '监听中';
    }
  }
  pill.dataset.state = state;
  $('statusText').textContent = text;

  const busy = stats.phase === 'syncing' || stats.phase === 'scanning';
  $('phaseBar').hidden = !busy;
  if (busy) {
    $('phaseText').textContent =
      stats.phase === 'scanning'
        ? '正在扫描本地目录…'
        : `正在同步，剩余 ${stats.pending} 个文件…`;
  }

  $('btnStart').disabled = running;
  $('btnStop').disabled = !running;
  $('btnFullSync').disabled = !running;

  if (stats.host) {
    $('targetLine').textContent = `${stats.host} → ${stats.remoteDir}`;
  } else {
    $('targetLine').textContent = '—';
  }

  $('lastSync').textContent = stats.lastSyncAt
    ? `上次全量同步：${new Date(stats.lastSyncAt).toLocaleString('zh-CN', { hour12: false })}`
    : '尚未完成同步';
}

/* ------------------------------------------------------------ 操作 */

async function saveConfig({ silent = false } = {}) {
  const cfg = readForm();
  const saved = await invoke('save_config', { config: cfg });
  writeForm(saved);
  if (!silent) toast('配置已保存', 'ok');
  return saved;
}

async function ensureSaved() {
  if (dirty) await saveConfig({ silent: true });
}

async function startSync() {
  try {
    await ensureSaved();
    const stats = await invoke('start_sync');
    applyStatus(stats);
    toast('已开始实时监听', 'ok');
  } catch (err) {
    toast(String(err), 'err');
  }
}

async function stopSync() {
  try {
    const stats = await invoke('stop_sync');
    applyStatus(stats);
    toast('已停止监听');
  } catch (err) {
    toast(String(err), 'err');
  }
}

async function fullSync() {
  try {
    await ensureSaved();
    await invoke('trigger_full_sync');
    toast('已加入全量同步任务');
  } catch (err) {
    toast(String(err), 'err');
  }
}

async function testConnection() {
  const btn = $('btnTest');
  btn.disabled = true;
  setTestResult('正在连接…', 'busy');
  try {
    const report = await invoke('test_connection', { config: readForm() });
    setTestResult(report, 'ok');
  } catch (err) {
    setTestResult(String(err), 'err');
  } finally {
    btn.disabled = false;
  }
}

async function pickLocal() {
  try {
    const picked = await invoke('pick_folder', { title: '选择本地同步目录' });
    if (picked) {
      $('localDir').value = picked;
      setDirty(true);
    }
  } catch (err) {
    toast(String(err), 'err');
  }
}

/* ------------------------------------------------------------ 事件绑定 */

function bind() {
  document.querySelectorAll('input, textarea').forEach((el) => {
    el.addEventListener('input', () => setDirty(true));
    el.addEventListener('change', () => setDirty(true));
  });

  $('btnSave').addEventListener('click', async () => {
    try {
      await saveConfig();
    } catch (err) {
      toast(String(err), 'err');
    }
  });

  $('btnStart').addEventListener('click', startSync);
  $('btnStop').addEventListener('click', stopSync);
  $('btnFullSync').addEventListener('click', fullSync);
  $('btnTest').addEventListener('click', testConnection);
  $('btnPickLocal').addEventListener('click', pickLocal);

  $('btnTogglePass').addEventListener('click', () => {
    const input = $('ftpPass');
    const show = input.type === 'password';
    input.type = show ? 'text' : 'password';
    $('btnTogglePass').textContent = show ? '隐藏' : '显示';
  });

  $('btnClearLogs').addEventListener('click', async () => {
    await invoke('clear_logs');
    renderLogs([]);
  });

  $('btnResetStats').addEventListener('click', async () => {
    await invoke('reset_stats');
    applyStatus(await invoke('get_status'));
    toast('统计计数已重置');
  });
}

/* ------------------------------------------------------------ 启动 */

async function boot() {
  bind();

  await listen('sync-log', (event) => appendLog(event.payload));
  await listen('sync-status', (event) => applyStatus(event.payload));

  try {
    const cfg = await invoke('load_config');
    writeForm(cfg);
  } catch (err) {
    toast('读取配置失败：' + err, 'err');
  }

  try {
    renderLogs(await invoke('get_logs'));
    applyStatus(await invoke('get_status'));
  } catch (err) {
    toast('初始化失败：' + err, 'err');
  }
}

boot();
