#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# One-time Heroku provisioning for mediaflow-proxy-light.
#
# Creates the app, configures config vars (auto-generating a 32-char API
# password), and enables automatic deploys from the GitHub `main` branch.
#
# Re-runnable: safe to re-execute if the app already exists.  Existing
# config vars are NOT overwritten (except RUST_VERSION, which is idempotent).
# ---------------------------------------------------------------------------
set -euo pipefail

APP_NAME="${HEROKU_APP_NAME:-}"
REGION="${HEROKU_REGION:-us}"
STACK="${HEROKU_STACK:-heroku-24}"
GITHUB_REPO="${HEROKU_GITHUB_REPO:-vskrch/mediaflow-proxy-light}"

red()    { printf '\033[31m%s\033[0m\n' "$*" >&2; }
green()  { printf '\033[32m%s\033[0m\n' "$*"; }
yellow() { printf '\033[33m%s\033[0m\n' "$*"; }

if ! command -v heroku >/dev/null 2>&1; then
  red "heroku CLI not found. Install from https://devcenter.heroku.com/articles/heroku-cli"
  exit 1
fi

if ! heroku auth:whoami >/dev/null 2>&1; then
  red "Not logged in. Run: heroku login"
  exit 1
fi

# --- App creation -----------------------------------------------------------
if [ -z "$APP_NAME" ]; then
  yellow "No HEROKU_APP_NAME set — letting Heroku auto-generate a name."
  CREATE_OUTPUT="$(heroku create --region "$REGION" --stack "$STACK" 2>&1)"
  echo "$CREATE_OUTPUT"
  APP_NAME="$(printf '%s\n' "$CREATE_OUTPUT" | sed -nE 's|.*https://git\.heroku\.com/([^.]+)\.git.*|\1|p' | head -n1)"
  if [ -z "$APP_NAME" ]; then
    red "Could not parse app name from 'heroku create' output."
    exit 1
  fi
else
  if heroku apps:info --app "$APP_NAME" >/dev/null 2>&1; then
    yellow "App '$APP_NAME' already exists — reusing it."
  else
    heroku create "$APP_NAME" --region "$REGION" --stack "$STACK"
  fi
fi
green "Using app: $APP_NAME (region=$REGION, stack=$STACK)"

# --- Buildpack -------------------------------------------------------------
heroku buildpacks:set --app "$APP_NAME" \
  https://github.com/emk/heroku-buildpack-rust

# --- Stack sanity ----------------------------------------------------------
heroku stack:set --app "$APP_NAME" "$STACK"

# --- Config vars ------------------------------------------------------------
# Mapping Heroku's $PORT to the app's expected APP__SERVER__PORT.
# Heroku substitutes $VAR references in config values at runtime.
heroku config:set --app "$APP_NAME" \
  APP__SERVER__HOST="0.0.0.0" \
  APP__SERVER__PORT='$PORT' \
  APP__SERVER__WORKERS="1" \
  APP__LOG_LEVEL="info" \
  RUST_LOG="" \
  APP__PROXY__FOLLOW_REDIRECTS="true" \
  APP__HLS__PREBUFFER_SEGMENTS="3" \
  >/dev/null

# Generate the API password only if not already set.
if heroku config:get --app "$APP_NAME" APP__AUTH__API_PASSWORD >/dev/null 2>&1 \
   && [ -n "$(heroku config:get --app "$APP_NAME" APP__AUTH__API_PASSWORD 2>/dev/null || true)" ]; then
  yellow "APP__AUTH__API_PASSWORD already set — keeping existing value."
else
  if command -v openssl >/dev/null 2>&1; then
    PASSWORD="$(openssl rand -base64 32 | tr -d '\n' | tr -d '/+=' | head -c 48)"
  else
    PASSWORD="$(LC_ALL=C tr -dc 'A-Za-z0-9' </dev/urandom | head -c 48)"
  fi
  heroku config:set --app "$APP_NAME" APP__AUTH__API_PASSWORD="$PASSWORD" >/dev/null
  green "Generated new APP__AUTH__API_PASSWORD (48 chars)."
fi

# --- GitHub auto-deploy ----------------------------------------------------
# Requires the Heroku GitHub integration to be authorised for the account.
# The CLI returns non-zero if the integration is not yet authorised — we
# fall back to a manual connect instruction in that case.
if heroku automation:enable --app "$APP_NAME" --repository "https://github.com/${GITHUB_REPO}.git" 2>/dev/null; then
  green "Auto-deploys enabled from github.com/${GITHUB_REPO} (main branch)."
else
  yellow "Could not enable auto-deploys via CLI."
  yellow "  -> Connect manually: https://dashboard.heroku.com/apps/${APP_NAME}/deploy/github"
  yellow "     - Deployment method: GitHub"
  yellow "     - Repo: ${GITHUB_REPO}"
  yellow "     - Enable Automatic Deploys from 'main'"
fi

# --- First push ------------------------------------------------------------
# Detect the current git branch.
CURRENT_BRANCH="$(git rev-parse --abbrev-ref HEAD 2>/dev/null || echo main)"
yellow "Triggering initial deploy from branch '$CURRENT_BRANCH'..."
heroku git:remote --app "$APP_NAME"
git push "https://git.heroku.com/${APP_NAME}.git" "HEAD:refs/heads/main" --force

green "Deploy complete. Tail logs with: heroku logs --tail --app $APP_NAME"
green "Open the app with:            heroku open --app $APP_NAME"
green "Health check:                 https://${APP_NAME}.herokuapp.com/health"
