# aynur-deploy

`aynur-deploy` 接收 Gitee Tag Push WebHook，按 Tag 对指定仓库做原子发布，并在健康检查失败时自动回滚。一个服务可以管理多个项目，部署任务按 FIFO 顺序串行执行。

## 安装与首次配置

生产机只安装已经在兼容的 Linux 主机上编译好的二进制，不在生产机编译 Rust。默认配置目录是 `/etc/aynur-deploy`，特殊安装目录可通过 `--home` 覆盖。

```bash
cargo install aynur-deploy --locked
sudo aynur-deploy init
sudo aynur-deploy add orhan-blog
```

`init` 首次运行创建 `config.toml` 和 `projects/`。随后用 `add <projectId>` 创建项目配置；每个项目都会自动生成自己的 `webhookToken`。编辑项目 TOML，至少填写正确的 `repositoryFullName`、`repositoryUrl` 和健康检查 URL：

```toml
projectId = "orhan-blog"
repositoryFullName = "aynurcn/blog_public"
repositoryUrl = "https://gitee.com/aynurcn/blog_public.git"
webhookToken = "由 add 自动生成的随机密码"
tagPattern = "^deploy-[0-9]{8}-[0-9]{6}$"
retainReleases = 3

[healthCheck]
url = "https://orhan.cn/95194bdc694f0283a5e145363cbbcf42.txt"
attempts = 5
intervalMs = 2000
timeoutMs = 5000

[deployment]
type = "static"
entryFile = "index.html"
```

把 `webhookToken` 填入 Gitee 仓库的 Tag Push WebHook Password，URL 指向反向代理后的 `/v1/hooks/gitee/orhan-blog`。Token 不会写入日志；项目 TOML 必须保持 `0600`。

## 启动与操作

服务的进程守护由 systemd 或 Aynur 负责，`aynur-deploy` 自身只负责部署。检查配置并运行服务：

```bash
aynur-deploy check
aynur-deploy serve
```

生产环境也可以让 systemd 运行同一个 `serve` 命令：

```bash
sudo systemctl enable --now aynur-deploy
aynur-deploy status orhan-blog
```

新增项目时执行 `add`，编辑生成的 TOML 后重启服务使配置生效：

```bash
sudo aynur-deploy add another-project
sudo systemctl restart aynur-deploy
```

其他运维命令：

```bash
aynur-deploy retry <deploymentId>
aynur-deploy rollback orhan-blog <commitSha>
aynur-deploy unblock orhan-blog
```

默认监听 `127.0.0.1:9091`；生产环境由 Nginx 提供 HTTPS 并转发 WebHook。`GET /healthz` 只检查服务和 SQLite 是否可用。
