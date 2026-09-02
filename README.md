# aynur-deploy

`aynur-deploy` 接收 Gitee Tag Push WebHook，按 Tag 对指定仓库做原子发布，并在健康检查失败时自动回滚。一个服务可以管理多个项目，部署任务按 FIFO 顺序串行执行。

## 安装与首次配置

生产机只安装已经在兼容的 Linux 主机上编译好的二进制，不在生产机编译 Rust。默认配置目录是 `/etc/aynur-deploy`；只有首次 `init` 时需要用 `--home` 指定特殊目录，后续命令会自动发现它。

```bash
cargo install aynur-deploy --locked
sudo aynur-deploy init
sudo aynur-deploy add orhan-blog
```

自定义目录只需指定一次：

```bash
aynur-deploy init --home ~/.config/aynur-deploy
aynur-deploy add orhan-blog
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
url = "https://orhan.cn/"
attempts = 5
intervalMs = 2000
timeoutMs = 5000

[deployment]
type = "static"
entryFile = "index.html"
```

`static` 是默认类型，适合 Zola、React/Vite 等已经生成最终静态目录的项目。React 项目应在独立的构建环境或 CI 中完成构建，把 `dist/`（或项目实际产物目录）作为发布内容，再让 `static.entryFile` 指向入口文件；部署服务不执行 Node 构建脚本。

如果仓库已经包含编译好的可执行文件：

```bash
aynur-deploy add my-service --type binary
```

生成的配置中填写二进制相对路径：

```toml
[deployment]
type = "binary"
binaryPath = "bin/my-service"
```

如果要在部署机从 Rust 源码构建：

```bash
aynur-deploy add my-service --type rust
```

生成的配置中填写 Cargo 参数：

```toml
[deployment]
type = "rust"
cargoManifest = "Cargo.toml"
package = "my-service"
binary = "my-service"
```

Node SSR 服务不属于 `static`：应先在构建环境产出可部署的二进制或其他固定运行产物，再使用 `binary`；服务编排由 Aynur 等独立进程管理器负责。

需要在切换发布后执行进程 reload 时，额外添加独立配置。命令使用固定 argv，可以使用 Aynur、PM2 或其他受信任的进程管理器，不经过 Shell：

```toml
[reload]
command = ["aynur", "reload", "orhan-api", "--update-env"]
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
