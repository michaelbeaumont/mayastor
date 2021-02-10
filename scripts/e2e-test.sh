#!/usr/bin/env bash

set -eu

SCRIPTDIR=$(dirname "$(realpath "$0")")
TESTDIR=$(realpath "$SCRIPTDIR/../test/e2e")
REPORTSDIR=$(realpath "$SCRIPTDIR/..")

# List and Sequence of tests.
#tests="install basic_volume_io csi replica rebuild node_disconnect/replica_pod_remove uninstall"
# Restrictions:
#   1. resource_check MUST follow csi
#       resource_check is a follow up check for the 3rd party CSI test suite.
#   2. replicas_pod_remove SHOULD be the last test before uninstall
#       this is a disruptive test.
#TESTS="install basic_volume_io csi replica rebuild node_disconnect/replica_pod_remove uninstall"
ALL_TESTS="install basic_volume_io csi resource_check replica rebuild uninstall"
ONDEMAND_TESTS="install basic_volume_io csi resource_check uninstall"
EXTENDED_TESTS="install basic_volume_io csi resource_check uninstall"
CONTINUOUS_TESTS="install basic_volume_io csi resource_check replica rebuild uninstall"

# Global state variables
#  test configuration state variables
device=
registry=
tag="ci"
#  script state variables
tests=""
custom_tests=""
profile="default"
on_fail="stop"
uninstall_cleanup="n"
generate_logs=0
logsdir=""

help() {
  cat <<EOF
Usage: $0 [OPTIONS]

Options:
  --device <path>           Device path to use for storage pools.
  --registry <host[:port]>  Registry to pull the mayastor images from.
  --tag <name>              Docker image tag of mayastor images (default "ci")
  --tests <list of tests>   Lists of tests to run, delimited by spaces (default: "$tests")
        Note: the last 2 tests should be (if they are to be run)
             node_disconnect/replica_pod_remove uninstall
  --profile <continuous|extended|ondemand>
                            Run the tests corresponding to the profile (default: run all tests)
  --reportsdir <path>       Path to use for junit xml test reports (default: repo root)
  --logs                    Generate logs and cluster state dump at the end of successful test run,
                            prior to uninstall.
  --logsdir <path>          Location to generate logs (default: emit to stdout).
  --onfail <stop|continue>  On fail, stop immediately or continue default($on_fail)
                            Behaviour for "continue" only differs if uninstall is in the list of tests (the default).
  --uninstall_cleanup <y|n> On uninstall cleanup for reusable cluster. default($uninstall_cleanup)

Examples:
  $0 --device /dev/nvme0n1 --registry 127.0.0.1:5000 --tag a80ce0c
EOF
}

# Parse arguments
while [ "$#" -gt 0 ]; do
  case "$1" in
    -d|--device)
      shift
      device=$1
      ;;
    -r|--registry)
      shift
      registry=$1
      ;;
    -t|--tag)
      shift
      tag=$1
      ;;
    -T|--tests)
      shift
      custom_tests="$1"
      ;;
    -R|--reportsdir)
      shift
      REPORTSDIR="$1"
      ;;
    -h|--help)
      help
      exit 0
      ;;
    --logs)
      generate_logs=1
      ;;
    --logsdir)
      shift
      logsdir="$1"
      if [[ "${logsdir:0:1}" == '.' ]]; then
          logsdir="$PWD/$logsdir"
      fi
      ;;
    --profile)
      shift
      profile="$1"
      ;;
    --onfail)
        shift
        case $1 in
            continue)
                on_fail=$1
                ;;
            stop)
                on_fail=$1
                ;;
            *)
                help
                exit 2
        esac
      ;;
    --uninstall_cleanup)
        shift
        case $1 in
            y|n)
                uninstall_cleanup=$1
                ;;
            *)
                help
                exit 2
        esac
      ;;
    *)
      echo "Unknown option: $1"
      help
      exit 1
      ;;
  esac
  shift
done

if [ -z "$device" ]; then
  echo "Device for storage pools must be specified"
  help
  exit 1
fi
export e2e_pool_device=$device

if [ -n "$tag" ]; then
  export e2e_image_tag="$tag"
fi

export e2e_docker_registry="$registry" # can be empty string

if [ -n "$custom_tests" ]; then
  if [ "$profile" != "default" ]; then
    echo "cannot specify --profile with --tests"
    help
    exit 1
  fi
  profile="custom"
fi

case "$profile" in
  continuous)
    tests="$CONTINUOUS_TESTS"
    ;;
  extended)
    tests="$EXTENDED_TESTS"
    ;;
  ondemand)
    tests="$ONDEMAND_TESTS"
    ;;
  custom)
    tests="$custom_tests"
    ;;
  default)
    tests="$ALL_TESTS"
    ;;
  *)
    echo "Unknown profile: $profile"
    help
    exit 1
    ;;
esac

export e2e_reports_dir="$REPORTSDIR"
if [ ! -d "$e2e_reports_dir" ] ; then
    echo "Reports directory $e2e_reports_dir does not exist"
    exit 1
fi

if [ "$uninstall_cleanup" == 'n' ] ; then
    export e2e_uninstall_cleanup=0
else
    export e2e_uninstall_cleanup=1
fi

test_failed=0

# Run go test in directory specified as $1 (relative path)
function runGoTest {
    pushd "$TESTDIR"
    echo "Running go test in $PWD/\"$1\""
    if [ -z "$1" ] || [ ! -d "$1" ]; then
        echo "Unable to locate test directory  $PWD/\"$1\""
        popd
        return 1
    fi

    cd "$1"
    if ! go test -v . -ginkgo.v -ginkgo.progress -timeout 0; then
        generate_logs=1
        popd
        return 1
    fi

    popd
    return 0
}

# Check if $2 is in $1
contains() {
    [[ $1 =~ (^|[[:space:]])$2($|[[:space:]]) ]] && return 0  || return 1
}

echo "Environment:"
echo "    e2e_pool_device=$e2e_pool_device"
echo "    e2e_image_tag=$e2e_image_tag"
echo "    e2e_docker_registry=$e2e_docker_registry"
echo "    e2e_reports_dir=$e2e_reports_dir"
echo "    e2e_uninstall_cleanup=$e2e_uninstall_cleanup"
echo ""
echo "Script control settings:"
echo "    profile=$profile"
echo "    on_fail=$on_fail"
echo "    uninstall_cleanup=$uninstall_cleanup"
echo "    generate_logs=$generate_logs"
echo "    logsdir=$logsdir"
echo ""
echo "list of tests: $tests"
for testname in $tests; do
  # defer uninstall till after other tests have been run.
  if [ "$testname" != "uninstall" ] ;  then
      if ! runGoTest "$testname" ; then
          echo "Test \"$testname\" Failed!!"
          test_failed=1
          break
      fi

      if ! ("$SCRIPTDIR/e2e_check_pod_restarts.sh") ; then
          echo "Test \"$testname\" Failed!! mayastor pods were restarted."
          test_failed=1
          generate_logs=1
          break
      fi

  fi
done

if [ "$generate_logs" -ne 0 ]; then
    if [ -n "$logsdir" ]; then
        if ! "$SCRIPTDIR/e2e-cluster-dump.sh" --destdir "$logsdir" ; then
            # ignore failures in the dump script
            :
        fi
    else
        if ! "$SCRIPTDIR/e2e-cluster-dump.sh" ; then
            # ignore failures in the dump script
            :
        fi
    fi
fi

if [ "$test_failed" -ne 0 ] && [ "$on_fail" == "stop" ]; then
    exit 3
fi

# Always run uninstall test if specified
if contains "$tests" "uninstall" ; then
    if ! runGoTest "uninstall" ; then
        echo "Test \"uninstall\" Failed!!"
        test_failed=1
        # Dump to the screen only, we do NOT want to overwrite
        # logfiles that may have been generated.
        if ! "$SCRIPTDIR/e2e-cluster-dump.sh" --clusteronly ; then
            # ignore failures in the dump script
            :
        fi
    fi
fi

if [ "$test_failed" -ne 0 ]; then
    echo "At least one test has FAILED!"
    exit 1
fi

echo "All tests have PASSED!"
exit 0
