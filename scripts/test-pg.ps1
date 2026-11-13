# SPDX-License-Identifier: GPL-2.0-or-later
<#
.SYNOPSIS
  Creates (or recreates) a PostgreSQL container used only by the backend
  tests. Default: oxledger-pg-test on 127.0.0.1:55433
  (docs/impl/testing-playbook.md).

.DESCRIPTION
  - Uses the local postgres:18 image and never pulls (--pull never); if the
    image is missing the script stops and says so.
  - Not durable on purpose: fsync, synchronous_commit and full_page_writes are
    off and the data directory is a tmpfs, so a container restart starts
    from an empty cluster (test_pool() recreates its databases).
  - One container per test suite. The search/notification horizon is shared
    by the whole cluster, so two suites on one container disturb each other
    whatever nextest isolates inside one run. A verifier runs its own
    container: -Name oxledger-pg-test-verify -Port 55434, with
    OXLEDGER_TEST_DATABASE_URL pointing at it.
  - Only ever touches the container named by -Name, which must start with
    oxledger-pg-test. The development container oxledger-pg (55432) is left
    alone.

.PARAMETER Name
  Container name; must start with oxledger-pg-test. Default oxledger-pg-test.

.PARAMETER Port
  Host port on 127.0.0.1. Default 55433.

.PARAMETER Recreate
  Remove the container named by -Name (and its data) first.

.EXAMPLE
  pwsh scripts/test-pg.ps1            # create if missing, start if stopped
  pwsh scripts/test-pg.ps1 -Recreate  # drop everything and start fresh
  pwsh scripts/test-pg.ps1 -Name oxledger-pg-test-verify -Port 55434  # verifier
#>
param(
    [string]$Name = 'oxledger-pg-test',
    [int]$Port = 55433,
    [switch]$Recreate
)

$ErrorActionPreference = 'Stop'
if (-not $Name.StartsWith('oxledger-pg-test')) {
    throw "container name '$Name' must start with oxledger-pg-test; this script never touches other containers"
}
$Image = 'postgres:18'
$MaxConnections = 300

function Invoke-Docker {
    & docker @args
    if ($LASTEXITCODE -ne 0) { throw "docker $($args -join ' ') failed ($LASTEXITCODE)" }
}

& docker image inspect $Image *> $null
if ($LASTEXITCODE -ne 0) {
    throw "image $Image is not available locally; this script does not pull images"
}

$exists = (& docker ps -a --filter "name=^/$Name$" --format '{{.Names}}') -eq $Name
if ($exists -and $Recreate) {
    Write-Host "removing $Name"
    Invoke-Docker rm -f $Name | Out-Null
    $exists = $false
}

if ($exists) {
    $running = (& docker inspect -f '{{.State.Running}}' $Name) -eq 'true'
    if (-not $running) {
        Write-Host "starting $Name"
        Invoke-Docker start $Name | Out-Null
    }
} else {
    Write-Host "creating $Name on 127.0.0.1:$Port"
    Invoke-Docker run -d --pull never --name $Name `
        -e POSTGRES_USER=oxledger -e POSTGRES_PASSWORD=oxledger_dev -e POSTGRES_DB=oxledger `
        -p "127.0.0.1:${Port}:5432" `
        --tmpfs '/var/lib/postgresql:rw,size=4g' `
        --shm-size=512m `
        $Image `
        -c fsync=off -c synchronous_commit=off -c full_page_writes=off `
        -c max_connections=$MaxConnections | Out-Null
}

# The entrypoint restarts the server once after initdb; wait for the final one.
for ($i = 0; $i -lt 60; $i++) {
    & docker exec $Name pg_isready -q -h 127.0.0.1 -U oxledger -d oxledger *> $null
    if ($LASTEXITCODE -eq 0) {
        $settings = & docker exec $Name psql -U oxledger -d oxledger -At -c "SELECT string_agg(name || '=' || setting, ' ' ORDER BY name) FROM pg_settings WHERE name IN ('fsync','synchronous_commit','full_page_writes','max_connections')"
        Write-Host "$Name ready on 127.0.0.1:${Port}: $settings"
        exit 0
    }
    Start-Sleep -Milliseconds 500
}
throw "$Name did not become ready within 30 s (docker logs $Name)"
