@echo off
setlocal

set "VSWHERE=%ProgramFiles(x86)%\Microsoft Visual Studio\Installer\vswhere.exe"
if exist "%VSWHERE%" (
  for /f "usebackq tokens=*" %%i in (`"%VSWHERE%" -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath`) do set "VSINSTALL=%%i"
)

if not defined VSINSTALL (
  echo Visual Studio with the MSVC x64 toolchain was not found.
  exit /b 1
)

call "%VSINSTALL%\VC\Auxiliary\Build\vcvars64.bat" >nul
if errorlevel 1 exit /b %errorlevel%

if "%~1"=="" goto check
if /i "%~1"=="check" goto check
if /i "%~1"=="test" goto test
if /i "%~1"=="clippy" goto clippy
if /i "%~1"=="release" goto release
if /i "%~1"=="package" goto package

echo Usage: build-local.bat [check^|test^|clippy^|release^|package]
exit /b 2

:check
cargo check --all-targets --locked
exit /b %errorlevel%

:test
cargo test --all-targets --locked
exit /b %errorlevel%

:clippy
cargo clippy --all-targets --all-features --locked -- -D warnings
exit /b %errorlevel%

:release
cargo build --release --locked
exit /b %errorlevel%

:package
cargo packager --release --formats nsis
exit /b %errorlevel%
