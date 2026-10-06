/* ============================================================
   界面预览用的 Tauri 桥接桩（仅用于本地/离线预览界面，不参与应用运行）
   通过 mock 掉 window.__TAURI__，让 app.js 可以在普通浏览器里跑起来，
   便于在没有桌面窗口的情况下检查布局、配色与交互状态。
   ============================================================ */

const demoConfig = {
  localDir: 'D:\\www\\site',
  remoteDir: '/www/wwwroot/site',
  mirrorDelete: true,
  cleanExtraneous: false,
  ignorePatterns: [
    '*.tmp',
    '*.swp',
    '*~',
    '~$*',
    '.DS_Store',
    'Thumbs.db',
    '.git',
    'node_modules',
  ],
  autoStart: true,
  syncOnStart: true,
  debounceMs: 800,
  ftp: {
    host: 'ftp.example.com',
    port: 21,
    username: 'deploy',
    password: 'demo-password',
    secure: true,
    timeoutSecs: 30,
  },
};

const demoStats = {
  running: true,
  phase: 'syncing',
  uploaded: 128,
  deleted: 6,
  skipped: 412,
  failed: 0,
  pending: 37,
  totalFiles: 583,
  lastError: '',
  lastSyncAt: Date.now() - 12 * 60 * 1000,
  localDir: demoConfig.localDir,
  remoteDir: demoConfig.remoteDir,
  host: demoConfig.ftp.host,
};

const demoLogs = [
  ['info', '配置文件目录: C:\\Users\\demo\\AppData\\Roaming\\com.hougang.ftpsync'],
  ['ok', '实时监听已启动'],
  ['ok', '正在监听 D:\\www\\site'],
  ['info', '开始全量同步：扫描本地目录…'],
  ['info', '本地共 583 个文件'],
  ['info', '需要上传 165 个文件'],
  ['ok', '↑ index.html  (4 218 字节)'],
  ['ok', '↑ assets/app.7f3c2a.js  (86 412 字节)'],
  ['ok', '↑ assets/app.7f3c2a.css  (12 908 字节)'],
  ['ok', '↑ uploads/2026/logo.png  (24 106 字节)'],
  ['info', '检测到 1 处变更'],
  ['ok', '✕ dist/legacy/old.js'],
  ['info', '· tmp/cache.json 远端已不存在，跳过删除'],
  ['ok', '↑ data/products.json  (512 337 字节)'],
  ['info', '检测到 3 处变更'],
];

const logsQueue = demoLogs.map(([level, message], i) => ({
  ts: Date.now() - (demoLogs.length - i) * 4200,
  level,
  message,
}));

const listeners = {};

function emit(name, payload) {
  (listeners[name] || []).forEach((cb) => cb({ event: name, payload }));
}

window.__TAURI__ = {
  core: {
    async invoke(cmd, args) {
      switch (cmd) {
        case 'load_config':
          return demoConfig;
        case 'save_config':
          return args && args.config ? args.config : demoConfig;
        case 'get_status':
          return demoStats;
        case 'get_logs':
          return logsQueue;
        case 'pick_folder':
          return 'D:\\www\\site';
        case 'test_connection':
          return '连接正常。服务器登录目录 /home/deploy，同步根目录 [/www/wwwroot/site] 下有 24 个条目';
        case 'start_sync':
        case 'stop_sync':
          return demoStats;
        default:
          return null;
      }
    },
  },
  event: {
    async listen(name, cb) {
      (listeners[name] = listeners[name] || []).push(cb);
      return () => {};
    },
  },
};

// 模拟运行中的状态推送，让预览能反映「同步进行中」的动态效果
setTimeout(() => {
  demoStats.uploaded += 1;
  demoStats.pending = Math.max(0, demoStats.pending - 1);
  logsQueue.push({
    ts: Date.now(),
    level: 'ok',
    message: '↑ assets/dashboard.9b1e04.js  (41 772 字节)',
  });
  emit('sync-log', logsQueue[logsQueue.length - 1]);
  emit('sync-status', demoStats);
}, 900);
