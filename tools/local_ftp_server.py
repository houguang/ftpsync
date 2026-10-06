"""本地测试用 FTP 服务器（仅用于自测，不属于应用组成部分）。

依赖：pip install pyftpdlib

用法：
    python local_ftp_server.py                 # 监听 127.0.0.1:2121
    FTP_ROOT=D:/tmp/ftproot python local_ftp_server.py

默认账号：test / test123，根目录 ./_ftp_root
"""

import os
import sys

try:
    from pyftpdlib.authorizers import DummyAuthorizer
    from pyftpdlib.handlers import FTPHandler
    from pyftpdlib.servers import FTPServer
except ImportError:
    sys.exit("缺少依赖，请先执行: pip install pyftpdlib")

HOST = os.environ.get("FTP_HOST", "127.0.0.1")
PORT = int(os.environ.get("FTP_PORT", "2121"))
USER = os.environ.get("FTP_USER", "test")
PASSWORD = os.environ.get("FTP_PASS", "test123")
ROOT = os.path.abspath(
    os.environ.get("FTP_ROOT", os.path.join(os.path.dirname(__file__), "_ftp_root"))
)

os.makedirs(ROOT, exist_ok=True)

authorizer = DummyAuthorizer()
# elradfmwMT = 目录切换/列表/读取/追加/删除/重命名/建目录/删除目录/改权限/改时间
authorizer.add_user(USER, PASSWORD, ROOT, perm="elradfmwMT")

handler = FTPHandler
handler.authorizer = authorizer
handler.passive_ports = range(60000, 60040)

server = FTPServer((HOST, PORT), handler)
print(f"FTP 服务器已启动: ftp://{HOST}:{PORT}  用户 {USER}/{PASSWORD}")
print(f"根目录: {ROOT}")
server.serve_forever()
