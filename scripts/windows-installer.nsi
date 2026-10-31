; Mini-Term GPUI 版 Windows NSIS 安装器(release.yml 的 Windows 线用 makensis 编译)。
;
; 身份对齐旧 Tauri NSIS(currentUser 模式):卸载注册表键沿用
; HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall\Mini-Term,并经
; InstallDirRegKey 读旧 InstallLocation —— 老用户运行新安装器默认落回原目录,
; 同名键覆盖写入,原地升级不留双条目(旧 Tauri 的 uninstall.exe 也被本包的
; 覆盖,注册表里那条 UninstallString 始终指向在场的卸载器)。
;
; 升级不再是「文件覆盖写」:启动时读注册表认出已装版本 → 弹框告知要先卸载
; (取消即退出安装器) → 用户在安装页按下开始后,先跑旧版自己的卸载器,再铺新
; 文件。这样旧版有、新版没有的残留文件不会留在安装目录里。用户数据在 AppData
; 下,卸载器不碰,升级不丢配置。详见 UNINSTALL_OLD 宏的注释。
;
; 按下开始后第一件事是清场:安装目录下有 mini-term 在跑就先问一句,同意后请它正常
; 退出、5 秒没退再强杀;别处的实例(dev 构建、其它副本)一概不碰。详见 CloseRunning。
;
; 包内布局 = 运行时布局:mini-term.exe + 三个 sidecar + portable-conpty\ 全部
; 平铺 $INSTDIR,与便携解压、target\<profile>\ 开发布局同构(「与 exe 同目录」
; 定位铁律)。用户数据在 AppData 下,卸载不碰。
;
; 真卸载时顺带摘掉 mini-term 写进五家 AI 工具(Claude / Codex / Grok / oh-my-pi / pi)配置里的
; hook 注册(`mini-term.exe --unregister-hooks`),升级时不摘 —— 新安装器调旧卸载器带
; /UPGRADE 区分,详见 UNINSTALL_OLD 宏与 Section "Uninstall" 的注释。
;
; 桌面快捷方式是组件页上的可选项(SecDesktop):全新安装默认勾上;升级时沿用
; 用户上一次的选择 —— .onInit 里看旧桌面快捷方式在不在(那时旧版还没卸),不在
; 就默认不勾。静默安装(/S)走的就是这个默认值。开始菜单快捷方式始终建,它是
; 卸载入口之一,不给选。
;
; 编译期必须 /D 传入(全部绝对路径):
;   VERSION      完整语义版本(如 1.0.0-beta,进注册表 DisplayVersion)
;   VERSION_NUM  纯数字四段(如 1.0.0.0,VIProductVersion 只收这个)
;   SOURCE_DIR   产物目录(target\release,已由 stage-sidecars.mjs 就位齐)
;   ICON_FILE    安装器图标(crates\mt-app\resources\icon.ico)
;   OUT_FILE     产物 setup.exe 输出路径

Unicode true
!include "MUI2.nsh"
!include "FileFunc.nsh"
!include "LogicLib.nsh"
; SelectSection / UnselectSection(.onInit 里按旧现场定桌面快捷方式的默认勾选)
!include "Sections.nsh"
; RunningX64 / DisableX64FSRedirection(关闭运行中实例时要起 64 位 PowerShell)
!include "x64.nsh"

!ifndef VERSION
  !error "makensis 需要 /DVERSION=<semver>"
!endif
!ifndef VERSION_NUM
  !error "makensis 需要 /DVERSION_NUM=<x.y.z.w>"
!endif
!ifndef SOURCE_DIR
  !error "makensis 需要 /DSOURCE_DIR=<target\release 绝对路径>"
!endif
!ifndef ICON_FILE
  !error "makensis 需要 /DICON_FILE=<icon.ico 绝对路径>"
!endif
!ifndef OUT_FILE
  !error "makensis 需要 /DOUT_FILE=<setup.exe 输出路径>"
!endif

!define PRODUCT_NAME "Mini-Term"
!define UNINST_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\${PRODUCT_NAME}"

; 已装旧版的现场(.onInit 探到,Section 用):空串 = 没装过,走全新安装。
Var OldUninstaller
Var OldInstallDir
Var OldVersion
Var UninstStatus
; CloseRunning 的入参:要清场的目录,`|` 分隔(Windows 路径里不会出现 `|`)。
Var KillDirs

Name "${PRODUCT_NAME}"
OutFile "${OUT_FILE}"
; 用户级安装,无 UAC —— 与旧 Tauri currentUser 模式一致;默认目录也取旧版
; 默认值($LOCALAPPDATA\Mini-Term),装过旧版的经 InstallDirRegKey 回原目录。
RequestExecutionLevel user
InstallDir "$LOCALAPPDATA\${PRODUCT_NAME}"
InstallDirRegKey HKCU "${UNINST_KEY}" "InstallLocation"
SetCompressor /SOLID lzma
ManifestDPIAware true

VIProductVersion "${VERSION_NUM}"
VIAddVersionKey /LANG=1033 "ProductName" "${PRODUCT_NAME}"
VIAddVersionKey /LANG=1033 "ProductVersion" "${VERSION}"
VIAddVersionKey /LANG=1033 "FileVersion" "${VERSION_NUM}"
VIAddVersionKey /LANG=1033 "FileDescription" "${PRODUCT_NAME} Installer"
VIAddVersionKey /LANG=1033 "LegalCopyright" "mini-term"

!define MUI_ICON "${ICON_FILE}"
!define MUI_UNICON "${ICON_FILE}"
!define MUI_ABORTWARNING

; 组件页只有两项(主程序必装 + 桌面快捷方式可选),右侧描述栏照给。
!insertmacro MUI_PAGE_COMPONENTS
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!define MUI_FINISHPAGE_RUN "$INSTDIR\mini-term.exe"
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

; 双语:运行时按系统界面语言自动挑选,不弹选择框。
!insertmacro MUI_LANGUAGE "English"
!insertmacro MUI_LANGUAGE "SimpChinese"

; 升级路径的三条文案(LangString 必须排在 MUI_LANGUAGE 之后;串里的 $Var 运行时展开)。
LangString MSG_OLD_FOUND ${LANG_ENGLISH} "${PRODUCT_NAME} $OldVersion is already installed in:$\r$\n$OldInstallDir$\r$\n$\r$\nIt will be uninstalled first, then ${VERSION} will be installed. Your settings and data (under AppData) are kept.$\r$\n$\r$\nClick OK to continue, or Cancel to quit."
LangString MSG_OLD_FOUND ${LANG_SIMPCHINESE} "检测到已安装 ${PRODUCT_NAME} $OldVersion,位置:$\r$\n$OldInstallDir$\r$\n$\r$\n将先卸载它,再安装 ${VERSION}。你的配置与数据(在 AppData 下)不会被删除。$\r$\n$\r$\n点「确定」继续,点「取消」退出安装。"
LangString MSG_UNINST_RUN ${LANG_ENGLISH} "Uninstalling ${PRODUCT_NAME} $OldVersion from $OldInstallDir ..."
LangString MSG_UNINST_RUN ${LANG_SIMPCHINESE} "正在卸载旧版 ${PRODUCT_NAME} $OldVersion($OldInstallDir)..."
LangString MSG_UNINST_FAIL ${LANG_ENGLISH} "The old version was not fully removed (uninstaller exit code: $UninstStatus).$\r$\n$\r$\nInstall ${VERSION} anyway (overwriting the existing files)?"
LangString MSG_UNINST_FAIL ${LANG_SIMPCHINESE} "旧版本没有卸载干净(卸载器退出码:$UninstStatus)。$\r$\n$\r$\n仍要继续安装 ${VERSION} 吗(直接覆盖现有文件)?"

; 组件页的两项名称与描述。
LangString SEC_MAIN_NAME ${LANG_ENGLISH} "${PRODUCT_NAME} (required)"
LangString SEC_MAIN_NAME ${LANG_SIMPCHINESE} "${PRODUCT_NAME} 主程序(必装)"
LangString SEC_MAIN_DESC ${LANG_ENGLISH} "The application, its helper programs and the Start Menu shortcut."
LangString SEC_MAIN_DESC ${LANG_SIMPCHINESE} "主程序、随附的辅助程序与开始菜单快捷方式。"
LangString SEC_DESKTOP_NAME ${LANG_ENGLISH} "Desktop shortcut"
LangString SEC_DESKTOP_NAME ${LANG_SIMPCHINESE} "桌面快捷方式"
LangString SEC_DESKTOP_DESC ${LANG_ENGLISH} "Create a ${PRODUCT_NAME} shortcut on the desktop."
LangString SEC_DESKTOP_DESC ${LANG_SIMPCHINESE} "在桌面上创建 ${PRODUCT_NAME} 快捷方式。"

; 安装目录里有主程序在跑时的询问框(安装 / 卸载各一条)与进度行。
LangString MSG_APP_RUNNING ${LANG_ENGLISH} "${PRODUCT_NAME} is running from the installation folder and has to be closed to continue. Its open terminals and AI sessions will end.$\r$\n$\r$\nClick Retry to let Setup close it: it is asked to exit normally first, and ended forcibly if it is still running after 5 seconds. You can also close it yourself first, then click Retry.$\r$\n$\r$\nClick Cancel to quit Setup."
LangString MSG_APP_RUNNING ${LANG_SIMPCHINESE} "安装目录中的 ${PRODUCT_NAME} 正在运行,继续安装需要先关闭它,其中打开的终端与 AI 会话会随之结束。$\r$\n$\r$\n点「重试」由安装器关闭它:先请它正常退出,5 秒后仍未退出则强制结束。也可以先自己关掉它再点「重试」。$\r$\n$\r$\n点「取消」退出安装。"
LangString MSG_APP_RUNNING_UNINST ${LANG_ENGLISH} "${PRODUCT_NAME} is running and has to be closed before it can be uninstalled. Its open terminals and AI sessions will end.$\r$\n$\r$\nClick Retry to let the uninstaller close it: it is asked to exit normally first, and ended forcibly if it is still running after 5 seconds. You can also close it yourself first, then click Retry.$\r$\n$\r$\nClick Cancel to quit."
LangString MSG_APP_RUNNING_UNINST ${LANG_SIMPCHINESE} "${PRODUCT_NAME} 正在运行,卸载前需要先关闭它,其中打开的终端与 AI 会话会随之结束。$\r$\n$\r$\n点「重试」由卸载程序关闭它:先请它正常退出,5 秒后仍未退出则强制结束。也可以先自己关掉它再点「重试」。$\r$\n$\r$\n点「取消」退出卸载。"
LangString MSG_CLOSING_RUNNING ${LANG_ENGLISH} "Closing ${PRODUCT_NAME} and its helper programs running from $KillDirs ..."
LangString MSG_CLOSING_RUNNING ${LANG_SIMPCHINESE} "正在关闭 $KillDirs 下运行的 ${PRODUCT_NAME} 及辅助程序..."
LangString MSG_CLOSE_UNAVAILABLE ${LANG_ENGLISH} "Could not check for running programs (PowerShell returned: $1). If a file is reported in use, close ${PRODUCT_NAME} and retry."
LangString MSG_CLOSE_UNAVAILABLE ${LANG_SIMPCHINESE} "没能检查运行中的程序(PowerShell 返回:$1)。如提示文件被占用,请先关闭 ${PRODUCT_NAME} 再重试。"

; 卸载时摘 AI 工具 hook 注册的进度行(见 Section "Uninstall")。
LangString MSG_UNREG_HOOKS ${LANG_ENGLISH} "Removing ${PRODUCT_NAME} hook entries from AI tool settings (Claude Code / Codex / Grok / oh-my-pi / pi) ..."
LangString MSG_UNREG_HOOKS ${LANG_SIMPCHINESE} "正在从 AI 工具配置(Claude Code / Codex / Grok / oh-my-pi / pi)中移除 ${PRODUCT_NAME} 的 hook 注册..."
LangString MSG_UNREG_HOOKS_FAIL ${LANG_ENGLISH} "Some hook entries could not be removed (result: $1). You can remove the miniterm-hook entries from those settings files by hand."
LangString MSG_UNREG_HOOKS_FAIL ${LANG_SIMPCHINESE} "部分 hook 注册未能移除(结果:$1),可手动删掉对应配置文件里含 miniterm-hook 的条目。"
LangString MSG_UNREG_HOOKS_KEEP ${LANG_ENGLISH} "Upgrading: AI tool hook entries are kept."
LangString MSG_UNREG_HOOKS_KEEP ${LANG_SIMPCHINESE} "升级安装:保留 AI 工具的 hook 注册。"

; ── 关闭安装目录下运行中的实例 ───────────────────────────────────────
;
; 升级 / 卸载要替换或删除 exe:主程序锁着 mini-term.exe;mt-ssh-cli 的 daemon、
; hook、被 AI CLI 拉起的 mt-ssh-mcp 同理。旧做法是无提示 `taskkill /F /IM <映像名>`,
; 三个问题:① 最常见的升级路径是「应用内提示更新 → 下载 → 应用还开着就运行安装器」,
; 所有终端与 AI 会话被一声不吭地强杀;② /F 绕过了应用退出时的配置排干(config_writer);
; ③ 按映像名匹配,连 dev 实例(target\...\mini-term.exe)和别处的副本一起杀。现在:
;
; - **只动路径落在清场目录下的进程**:安装目录;升级且用户改选了新目录时再加上旧版
;   目录(旧 Tauri 版的 Mini-Term.exe 也装在那里 —— 默认 $LOCALAPPDATA\Mini-Term,
;   自选过目录的由注册表 InstallLocation 带回 $OldInstallDir;进程名匹配不分大小写)。
; - **主程序先优雅关闭**:对主窗口发 WM_CLOSE(CloseMainWindow)。mini-term 收到它走的是
;   标题栏 ✕ 同一条路(title_bar::allow_close):没有在跑的 AI、没有未保存文件时立刻落盘
;   配置并退出,进程退出前 on_app_quit 再排干一次配置写线程;有风险项时 mini-term 自己
;   弹关窗确认框 —— 用户在 5 秒内点了确定就是正常退出,否则到点强杀。托盘不拦 WM_CLOSE
;   (托盘是另一个隐藏窗口,CloseMainWindow 不碰它;关窗即退出,没有「关到托盘」)。
;   所以不需要另开 IPC / 退出参数。
; - **sidecar 直接强杀**:它们没有窗口,也没有需要排干的状态;不结束就换不了文件。
; - 交互安装 / 卸载:探到主程序在跑先弹框(重试 = 由安装器关闭后继续;取消 = 退出);
;   静默(/S)不弹框,直接优雅关闭 → 5 秒 → 强杀。没在跑时不打扰。
;
; 实现走 PowerShell(-Command 的参数不受执行策略约束,策略只管脚本文件)。输入经环境
; 变量传,不拼进命令行,路径里的空格 / 中文 / 单引号都不用转义:
;   MT_KILL_DIRS  清场目录,`|` 分隔
;   MT_KILL_MODE  probe = 只数清场目录下的主程序实例,个数作退出码;
;                 close = 主程序 CloseMainWindow → 最多等 5 秒 → 没退的强杀;再强杀清场
;                         目录下的 sidecar;最后等它们真的退出(释放文件句柄)
; 进程路径取 Process.Path,取不到退回 CIM 的 ExecutablePath;目录统一补尾部 `\` 后按
; 前缀比(OrdinalIgnoreCase),不用 -like —— 路径里的 [ ] 会被当成通配符。
; ⚠️ 整条脚本进 nsExec 的命令行,受 NSIS_MAX_STRLEN(官方构建 1024 字符)限制,只能写紧;
; 下面按段拆开的只是写法,拼起来是同一行。改完用 makensis 编一次,超长会被运行时截断。
; 1) 清场目录清单,各补尾部 `\`(D:\a 不会误中 D:\ab)
!define CLOSE_PS_1 "$$d=@($$env:MT_KILL_DIRS -split '\|' | ?{$$_} | %{$$_.TrimEnd('\')+'\'});"
; 2) M <进程名...> = 这些名字里路径落在清单目录下的进程
!define CLOSE_PS_2 "function M($$n){@(Get-Process -Name $$n -EA 0 | ?{$$p=$$_.Path;if(!$$p){$$p=(Get-CimInstance Win32_Process -Filter ('ProcessId='+$$_.Id) -EA 0).ExecutablePath};$$h=0;foreach($$x in $$d){if($$p -and $$p.StartsWith($$x,'OrdinalIgnoreCase')){$$h=1}};$$h})};"
; 3) 主程序(新版 mini-term.exe / 旧 Tauri 版 Mini-Term.exe);probe 到此为止
!define CLOSE_PS_3 "$$m=@(M 'mini-term');if($$env:MT_KILL_MODE -eq 'probe'){exit $$m.Count};"
; 4) 发 WM_CLOSE,最多等 5 秒
!define CLOSE_PS_4 "foreach($$q in $$m){try{[void]$$q.CloseMainWindow()}catch{}};$$t=[DateTime]::Now.AddSeconds(5);while([DateTime]::Now -lt $$t -and @($$m | ?{!$$_.HasExited}).Count){Start-Sleep -m 200};"
; 5) 没退的主程序 + 全部 sidecar 强杀,再等它们真的退出
!define CLOSE_PS_5 "$$k=@($$m | ?{!$$_.HasExited})+@(M 'miniterm-hook','mt-ssh-cli','mt-ssh-mcp');$$k | Stop-Process -Force -EA 0;$$k | Wait-Process -Timeout 5 -EA 0;exit 0"

; 用 64 位 PowerShell 跑上面那条脚本,退出码(起不来是 "error",超时是 "timeout")进 ${OUT}。
; 安装器本身是 32 位进程,不关文件系统重定向的话 $SYSDIR 会被转到 SysWOW64、起的是 32 位
; PowerShell,读不到 64 位进程的 MainModule(Path 全空,只能靠 CIM 兜底,慢)。
!macro RUN_CLOSE_PS OUT
  ${If} ${RunningX64}
    ${DisableX64FSRedirection}
  ${EndIf}
  nsExec::Exec /TIMEOUT=60000 `"$SYSDIR\WindowsPowerShell\v1.0\powershell.exe" -NoProfile -NonInteractive -Command "${CLOSE_PS_1}${CLOSE_PS_2}${CLOSE_PS_3}${CLOSE_PS_4}${CLOSE_PS_5}"`
  Pop ${OUT}
  ${If} ${RunningX64}
    ${EnableX64FSRedirection}
  ${EndIf}
!macroend

; 清场 $KillDirs 下的 mini-term 主程序与 sidecar(口径见上)。安装器与卸载器各一份
; (卸载器里的函数必须以 un. 开头);MSG = 询问框用哪条文案。
!macro DEFINE_CLOSE_RUNNING UN MSG
Function ${UN}CloseRunning
  Push $0
  Push $1
  StrCpy $0 $KillDirs
  ; r0 = 取 $0 的值,不把路径拼进调用串(路径里的括号会搅乱 System 插件的解析)
  System::Call 'Kernel32::SetEnvironmentVariable(t "MT_KILL_DIRS", t r0)'
  ${IfNot} ${Silent}
    System::Call 'Kernel32::SetEnvironmentVariable(t "MT_KILL_MODE", t "probe")'
    !insertmacro RUN_CLOSE_PS $1
    ; $1 = 清场目录下主程序的实例数。PowerShell 起不来时是 "error",`>` 按整数比当 0 ——
    ; 探不到就不拦,真有文件被占用时 NSIS 自己的写文件失败框(重试 / 忽略)会接住
    ${If} $1 > 0
      MessageBox MB_RETRYCANCEL|MB_ICONEXCLAMATION "$(${MSG})" /SD IDRETRY IDRETRY +2
        Abort
    ${EndIf}
  ${EndIf}
  DetailPrint "$(MSG_CLOSING_RUNNING)"
  System::Call 'Kernel32::SetEnvironmentVariable(t "MT_KILL_MODE", t "close")'
  !insertmacro RUN_CLOSE_PS $1
  ${If} $1 != "0"
    DetailPrint "$(MSG_CLOSE_UNAVAILABLE)"
  ${EndIf}
  Pop $1
  Pop $0
FunctionEnd
!macroend
!insertmacro DEFINE_CLOSE_RUNNING "" MSG_APP_RUNNING
!insertmacro DEFINE_CLOSE_RUNNING "un." MSG_APP_RUNNING_UNINST

; 跑旧版自己的卸载器。带 `_?=` 是关键:没有它,NSIS 卸载器会先把自己复制到
; %TEMP% 再启动,ExecWait 等到的是那个立即返回的壳,新文件会和卸载动作打架。
; 代价是运行中的 uninstall.exe 删不掉自己,残留由本宏补删(旧 Tauri 版的卸载器
; 同样是 NSIS 出身,`_?=` 与 /S 都认)。
; 卸载器会清掉快捷方式与 Uninstall 注册表键:开始菜单那条与注册表键由 SecMain
; 后半段原样重建,桌面那条看 SecDesktop 勾没勾。
;
; /UPGRADE 告诉旧卸载器「这是升级,不是卸载」:真卸载会摘掉 AI 工具里的 hook 注册
; (见 Section "Uninstall"),升级时摘了,新版装好后 AI 状态感知就断了 —— 启动期自愈
; 只在「已注册过」时补,摘光之后它也救不回来。
; - 旧卸载器不认识 /UPGRADE:NSIS 卸载器只解析 /S、/NCRC、/D=、_?=,其余参数原样
;   留在命令行里无人理会;而且带 hook 清理之前的卸载器(GPUI 版到 1.13.7、Tauri 版)
;   本来就没有摘 hook 这一步,不会误摘。
; - `_?=` 必须是最后一个参数(它把后面整段当目录,允许带空格)。
!macro UNINSTALL_OLD
  ${If} $OldUninstaller != ""
    DetailPrint "$(MSG_UNINST_RUN)"
    ClearErrors
    ExecWait '"$OldUninstaller" /S /UPGRADE _?=$OldInstallDir' $UninstStatus
    ${If} ${Errors}
      StrCpy $UninstStatus "-1"
    ${EndIf}
    Delete "$OldUninstaller"
    ; 旧目录若已空就收掉;用户改选了新目录时这一步顺带清干净老位置。
    RMDir "$OldInstallDir"
    ${If} $UninstStatus != "0"
      ; 卸载失败不直接判死:静默安装默认继续覆盖,交互装由用户定。
      MessageBox MB_YESNO|MB_ICONEXCLAMATION "$(MSG_UNINST_FAIL)" /SD IDYES IDYES +2
        Abort
    ${EndIf}
  ${EndIf}
!macroend

; 主程序:SectionIn RO = 组件页上灰掉、永远勾着。
Section "$(SEC_MAIN_NAME)" SecMain
  SectionIn RO
  ; 先清场再卸载:旧卸载器同样要动这几个 exe。用户在目录页改选了新目录时,旧目录里
  ; 在跑的也要一起清(旧卸载器删的是那边的文件)。
  StrCpy $KillDirs "$INSTDIR"
  ${If} $OldInstallDir != ""
  ${AndIf} $OldInstallDir != "$INSTDIR"
    StrCpy $KillDirs "$INSTDIR|$OldInstallDir"
  ${EndIf}
  Call CloseRunning
  !insertmacro UNINSTALL_OLD

  SetOutPath "$INSTDIR"
  File "${SOURCE_DIR}\mini-term.exe"
  File "${SOURCE_DIR}\miniterm-hook.exe"
  File "${SOURCE_DIR}\mt-ssh-cli.exe"
  File "${SOURCE_DIR}\mt-ssh-mcp.exe"
  SetOutPath "$INSTDIR\portable-conpty"
  File /r "${SOURCE_DIR}\portable-conpty\*"
  SetOutPath "$INSTDIR"

  WriteUninstaller "$INSTDIR\uninstall.exe"
  CreateShortcut "$SMPROGRAMS\${PRODUCT_NAME}.lnk" "$INSTDIR\mini-term.exe"

  WriteRegStr HKCU "${UNINST_KEY}" "DisplayName" "${PRODUCT_NAME}"
  WriteRegStr HKCU "${UNINST_KEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "${UNINST_KEY}" "DisplayIcon" "$INSTDIR\mini-term.exe"
  WriteRegStr HKCU "${UNINST_KEY}" "Publisher" "mini-term"
  WriteRegStr HKCU "${UNINST_KEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKCU "${UNINST_KEY}" "UninstallString" '"$INSTDIR\uninstall.exe"'
  WriteRegDWORD HKCU "${UNINST_KEY}" "NoModify" 1
  WriteRegDWORD HKCU "${UNINST_KEY}" "NoRepair" 1
  ${GetSize} "$INSTDIR" "/S=0K" $0 $1 $2
  IntFmt $0 "0x%08X" $0
  WriteRegDWORD HKCU "${UNINST_KEY}" "EstimatedSize" $0
SectionEnd

; 桌面快捷方式:可选,默认勾选;升级时的默认值见 .onInit。
Section "$(SEC_DESKTOP_NAME)" SecDesktop
  CreateShortcut "$DESKTOP\${PRODUCT_NAME}.lnk" "$INSTDIR\mini-term.exe"
SectionEnd

; 组件页右侧的描述(宏要在 Section 之后展开,它引用 ${SecMain} / ${SecDesktop})。
!insertmacro MUI_FUNCTION_DESCRIPTION_BEGIN
  !insertmacro MUI_DESCRIPTION_TEXT ${SecMain} "$(SEC_MAIN_DESC)"
  !insertmacro MUI_DESCRIPTION_TEXT ${SecDesktop} "$(SEC_DESKTOP_DESC)"
!insertmacro MUI_FUNCTION_DESCRIPTION_END


; 认出已装版本并征得同意。放 .onInit 是为了让用户在第一屏就知道要先卸载;真正
; 的卸载动作留到 Section(用户在目录页仍可反悔,反悔时旧版原封不动)。
; 排在 Section 之后是因为它要引用 ${SecDesktop}(Section 编译到才有这个定义)。
Function .onInit
  ReadRegStr $0 HKCU "${UNINST_KEY}" "UninstallString"
  ${If} $0 == ""
    Return
  ${EndIf}
  ; 注册表里的 UninstallString 是带引号的命令行,剥成裸路径才能喂 ExecWait。
  StrCpy $1 $0 1
  StrCpy $2 $0 "" -1
  ${If} $1 == '"'
  ${AndIf} $2 == '"'
    StrCpy $0 $0 -1 1
  ${EndIf}
  ${IfNot} ${FileExists} "$0"
    ; 注册表在、卸载器不在(用户手删过目录):当全新安装处理,不拿假提示烦人。
    Return
  ${EndIf}
  StrCpy $OldUninstaller "$0"
  ; `_?=` 要的是卸载器自己所在的目录,所以取它的父目录而不是注册表里的
  ; InstallLocation(那条可能带尾反斜杠或被手工改过)。
  ${GetParent} "$0" $OldInstallDir
  ${If} $OldInstallDir == ""
    ReadRegStr $OldInstallDir HKCU "${UNINST_KEY}" "InstallLocation"
  ${EndIf}
  ReadRegStr $OldVersion HKCU "${UNINST_KEY}" "DisplayVersion"
  ${If} $OldVersion == ""
    StrCpy $OldVersion "?"
  ${EndIf}
  ${IfNot} ${Silent}
    MessageBox MB_OKCANCEL|MB_ICONINFORMATION "$(MSG_OLD_FOUND)" /SD IDOK IDOK +2
      Abort
  ${EndIf}
  ; 升级沿用上一次的选择:旧版装过桌面快捷方式就默认勾上,没装(或用户自己删了)
  ; 就默认不勾。此刻旧版还没卸,桌面上那个 .lnk 就是现场。
  ${IfNot} ${FileExists} "$DESKTOP\${PRODUCT_NAME}.lnk"
    !insertmacro UnselectSection ${SecDesktop}
  ${EndIf}
FunctionEnd

Section "Uninstall"
  ; 升级时由新安装器以 /S /UPGRADE 调起:那边已经清过场,这里多半什么都探不到
  StrCpy $KillDirs "$INSTDIR"
  Call un.CloseRunning

  ; 摘掉 mini-term 写进 AI 工具配置的 hook 注册。必须排在删 exe 之前(靠主程序自己
  ; 摘:摘除口径 —— 只认带 miniterm-hook 标识的条目、没有就一个字节不写 —— 与设置页
  ; 「卸载」同一份代码,见 crates/mt-app/src/cli.rs)。
  ; - 升级(/UPGRADE)不摘,理由见 UNINSTALL_OLD。
  ; - 静默卸载(用户自己 /S、包管理器)照样摘:那是真卸载,留着的条目会让 AI 每个事件
  ;   都去跑一个不存在的 exe;安装器发起的静默调用一律带 /UPGRADE,不会走到这里。
  ; - nsExec 起的子进程 stdout 接到详情列表(每家一行 ASCII);30 秒超时强杀兜底,
  ;   摘失败只提示、不中断卸载。
  ${un.GetParameters} $0
  ClearErrors
  ${un.GetOptions} $0 "/UPGRADE" $1
  ${If} ${Errors}
    ${If} ${FileExists} "$INSTDIR\mini-term.exe"
      DetailPrint "$(MSG_UNREG_HOOKS)"
      nsExec::ExecToLog /TIMEOUT=30000 '"$INSTDIR\mini-term.exe" --unregister-hooks'
      Pop $1
      ${If} $1 != "0"
        DetailPrint "$(MSG_UNREG_HOOKS_FAIL)"
      ${EndIf}
    ${EndIf}
  ${Else}
    DetailPrint "$(MSG_UNREG_HOOKS_KEEP)"
  ${EndIf}

  Delete "$INSTDIR\mini-term.exe"
  Delete "$INSTDIR\miniterm-hook.exe"
  Delete "$INSTDIR\mt-ssh-cli.exe"
  Delete "$INSTDIR\mt-ssh-mcp.exe"
  RMDir /r "$INSTDIR\portable-conpty"
  Delete "$INSTDIR\uninstall.exe"
  ; 只删空目录:用户自选目录里若有别的东西,不动。
  RMDir "$INSTDIR"

  Delete "$SMPROGRAMS\${PRODUCT_NAME}.lnk"
  Delete "$DESKTOP\${PRODUCT_NAME}.lnk"
  DeleteRegKey HKCU "${UNINST_KEY}"
SectionEnd
