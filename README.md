# aynur-deploy

`aynur-deploy` 接收 Gitee Tag Push WebHook，按 Tag 对指定仓库做原子发布，并在健康检查失败时自动回滚。一个服务可以管理多个项目，部署任务按 FIFO 顺序串行执行。

## 安装与首次配置

生产机只安装已经在兼容的 Linux 主机上编译好的二进制，不在生产机编译 Rust。默认配置目录是 `/etc/aynur-deploy`；只有首次 `init` 时需要用 `--home` 指定特殊目录，后续命令会自动发现它。

```bash
cargo install aynur-deploy --locked
sudo aynur-deploy init
sudo aynur-deploy add orhan-blog
```

`add` 会把默认发布软链路径写入项目配置：

```text
<stateDirectory>/projects/<projectId>/current
```

已有服务器需要保留原来的 Nginx `root` 或进程启动路径时，在 `add` 命令中显式指定：

```bash
sudo aynur-deploy add orhan-blog --current-path /var/www/blog_public
```

如果 `/var/www/blog_public` 是已有目录，`add` 会把它重命名为同级的 `/var/www/blog_public.before-aynur-deploy`，再在原路径创建指向该目录的绝对软链。原有内容不会复制或删除，命令输出中的 `bootstrapPath` 会记录备份位置。备份路径已存在或 `currentPath` 是普通文件时，命令会在移动内容前明确失败，并删除本次生成的项目配置。

自定义目录只需指定一次：

```bash
aynur-deploy init --home ~/.config/aynur-deploy
aynur-deploy add orhan-blog
```

`init` 首次运行创建 `config.toml` 和 `projects/`。随后用 `add <projectId>` 创建项目配置；每个项目都会自动生成自己的 `webhookToken`。编辑项目 TOML，至少填写正确的 `repositoryFullName`、`repositoryUrl` 和健康检查 URL：

```toml
projectId = "orhan-blog"
currentPath = "/var/www/blog_public"
repositoryFullName = "aynurcn/blog_public"
repositoryUrl = "git@gitee.com:aynurcn/blog_public.git"
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

`repositoryUrl` 可以使用 HTTPS、本地路径或 Git SSH 地址。生产环境推荐给运行 `aynur-deploy serve` 的系统用户配置只读 Deploy Key，并提前验证该用户的 `known_hosts`。无人值守服务不能依赖交互式密码或首次连接确认；带口令的私钥还需要服务进程可访问的 `ssh-agent`。不要把 HTTPS 用户名或 Token 嵌入 URL，因为 Git 命令失败时 URL 可能出现在错误日志中。修改已有项目的仓库地址时，还要同步修改 `stateDirectory/mirrors/<projectId>.git` 的 `origin`，服务会拒绝配置地址与 mirror 地址不一致的部署。

`currentPath` 是必填的绝对路径。`add` 未指定 `--current-path` 时会写入默认路径；指定后则原样写入项目 TOML，并自动接管该位置已有的目录。配置加载时，该路径必须不存在或已经是软链。release 仍保存在 `stateDirectory/projects/<projectId>/releases/<commitSha>`，部署时只原子切换 `currentPath` 软链。首次部署确认成功前不要删除 `bootstrapPath`，它是首次健康检查失败时的回滚目标。

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
includePaths = ["Toasty.toml", "profiles", "toasty"]
binaries = [
    { package = "gateway", binary = "gateway" },
    { package = "migrator", binary = "migrator" },
]
environmentFile = "/etc/my-service.env"
```

`binaries` 至少包含一个 `{ package, binary }`，目标文件名必须唯一。服务对所有目标执行一次 `cargo build --release --locked`，随后把指定二进制复制到 release 根目录。`includePaths` 相对于本次 Tag 的临时 Git worktree 根目录解析；文件原样复制，目录递归复制，缺失或越界路径会使部署失败。

`environmentFile` 是可选的生产环境文件绝对路径，必须是权限 `0600` 的普通文件。它不会进入 release，也不会写入日志；其中的变量提供给 Cargo build 和 migration，并覆盖部署服务进程中的同名变量。密钥、数据库和上传文件应保存在 release 目录之外。

需要在切换 release 前执行数据库迁移时，配置一条固定 argv 命令。命令只在正向部署中执行，工作目录是候选 release；迁移失败不会切换 `currentPath`。进程在迁移期间重启时会重新执行该命令，因此迁移工具必须能够安全重复执行：

```toml
[migration]
command = ["./migrator", "migration", "apply"]
```

数据库迁移不会在显式回滚或健康检查回滚时逆向执行。需要自动应用回滚的项目，migration 必须兼容上一版应用。

Node SSR 服务不属于 `static`：应先在构建环境产出可部署的二进制或其他固定运行产物，再使用 `binary`；服务编排由 Aynur 等独立进程管理器负责。

需要在切换发布后执行进程 reload 时，额外添加独立配置。多条命令按顺序执行、遇错停止，并在正向激活和应用回滚后执行。命令使用固定 argv，可以使用 Aynur、PM2 或其他受信任的进程管理器，不经过 Shell：

```toml
[reload]
commands = [
    ["aynur", "reload", "orhan-api", "--update-env"],
]
```

把 `webhookToken` 填入 Gitee 仓库的 Tag Push WebHook Password，URL 指向反向代理后的 `/v1/hooks/gitee/orhan-blog`。Token 不会写入日志；项目 TOML 必须保持 `0600`。

## Nginx 反向代理

一个 `aynur-deploy` 实例只需要一个 Nginx 入口，所有项目共用 `/v1/hooks/gitee/<projectId>`。下面的域名和证书路径是脱敏示例：

```nginx
# /etc/nginx/conf.d/aynur-deploy.conf
server {
    listen 80;
    listen [::]:80;
    server_name deploy.example.com;

    return 301 https://$host$request_uri;
}

server {
    listen 443 ssl;
    listen [::]:443 ssl;
    server_name deploy.example.com;

    ssl_certificate /etc/letsencrypt/live/deploy.example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/deploy.example.com/privkey.pem;

    # Matches maxWebhookBodyBytes = 65536.
    client_max_body_size 64k;

    location = /healthz {
        proxy_pass http://127.0.0.1:9091/healthz;
        proxy_connect_timeout 2s;
        proxy_read_timeout 5s;
        proxy_send_timeout 5s;
    }

    location ^~ /v1/hooks/gitee/ {
        proxy_pass http://127.0.0.1:9091;
        proxy_http_version 1.1;

        proxy_connect_timeout 2s;
        proxy_read_timeout 10s;
        proxy_send_timeout 10s;
    }

    location / {
        return 404;
    }
}
```

每个 Gitee 项目只需要把 WebHook URL 中的 `<projectId>` 换成项目配置里的 `projectId`，例如 `https://deploy.example.com/v1/hooks/gitee/orhan-blog`。Nginx 默认会转发 `X-Gitee-Token` 和 `X-Gitee-Ping` 请求头，不需要为每个项目增加 `location`。

## 启动与操作

服务的进程守护由 systemd 或 Aynur 负责，`aynur-deploy` 自身只负责部署。检查配置并运行服务：

```bash
aynur-deploy check
aynur-deploy serve
```

查看当前配置的所有部署项目、运行状态及其发布路径：

```bash
aynur-deploy list
```

生产环境也可以让 systemd 运行同一个 `serve` 命令：

```bash
sudo systemctl enable --now aynur-deploy
aynur-deploy status orhan-blog
aynur-deploy clean orhan-blog --keep 20 --type failed
```

新增项目时执行 `add`，编辑生成的 TOML 后重启服务使配置生效：

```bash
sudo aynur-deploy add another-project
sudo systemctl restart aynur-deploy
```

其他运维命令：

```bash
aynur-deploy stop orhan-blog
aynur-deploy start orhan-blog
aynur-deploy delete orhan-blog
aynur-deploy retry <deploymentId>
aynur-deploy rollback orhan-blog <commitSha>
aynur-deploy unblock orhan-blog
```

`clean` 只清理已经结束的部署历史和同 deployment ID 的残留 worktree/target。`--type` 必须是 `failed`、`succeeded` 或 `all`，`--keep` 指定匹配记录中保留的最新数量；存在进行中的部署时命令会拒绝执行，正式 release 不会被删除。

部署命令的 stdout 和 stderr 会按行写入服务结构化日志。由 Aynur 管理服务时，可用 `aynur logs aynur-deploy` 实时查看 Cargo 的 `Compiling` 和 `Finished` 输出。

`stop` 会持久化停止状态：新的 Tag WebHook 返回 `409 projectStopped`，排队任务暂停；已经开始构建、迁移或切换的任务会继续完成。`start` 恢复接收和处理，但不会解除部署失败产生的 `blocked` 状态，必须先处理故障并执行 `unblock`。

`delete` 只允许删除已经停止且没有执行中任务的项目。它会删除项目 TOML、项目状态和部署历史，但保留 `currentPath`、release、mirror 和其他发布文件，因此不会同时下线当前应用。运行中的 `aynur-deploy serve` 会立即通过数据库状态拒绝已删除项目，不要求重启。

默认监听 `127.0.0.1:9091`。`GET /healthz` 只检查服务和 SQLite 是否可用。
