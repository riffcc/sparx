#!/usr/bin/env bash

# Parse arguments
SHOW_INSTALL=false
for arg in "$@"; do
    case $arg in
        --show-install)
            SHOW_INSTALL=true
            shift
            ;;
    esac
done

# Setup output redirection based on verbosity
if [ "$SHOW_INSTALL" = true ]; then
    # When showing install, just run commands normally
    run_cmd() {
        "$@"
    }
else
    # When quiet, redirect output unless there's an error
    run_cmd() {
        # It's possible SHOW_INSTALL has been set to true since this function was set
        # So we need to check it again.
        if [ "$SHOW_INSTALL" = true ]; then
            "$@"
        else
            # Capture both stdout and stderr in temporary files
            local stdout_file=$(mktemp)
            local stderr_file=$(mktemp)
            
        if ! "$@" > "$stdout_file" 2> "$stderr_file"; then
            echo "Command failed: $*"
            echo "Error output:"
            cat "$stderr_file"
            echo "Standard output:"
            cat "$stdout_file"
            # Clean up temp files
            rm -f "$stdout_file" "$stderr_file"
            exit 1
            fi
            # Clean up temp files on success
            rm -f "$stdout_file" "$stderr_file"
        fi
    }
fi

# Check that Python3 and pip are installed
if ! command -v python3 &> /dev/null && ! command -v pip &> /dev/null; then
    # Detect OS and install Python3 and pip
    if [ -f /etc/lsb-release ]; then
        # Ubuntu/Debian
        echo "Installing Python3 and pip..."
        run_cmd sudo apt update
        run_cmd sudo apt install -y python3 python3-pip
    elif [ -f /etc/redhat-release ]; then
        # AlmaLinux/RockyLinux/Fedora
        echo "Installing Python3 and pip..."
        run_cmd sudo dnf install -y python3 python3-pip
    elif [[ "$OSTYPE" == "darwin"* ]]; then
        # macOS
        if ! command -v brew &> /dev/null; then
            echo "Homebrew not found. Would you like to install it? (y/n)"
            read -p "Enter your choice: " choice
            if [ "$choice" == "y" ]; then
                echo "Installing Homebrew..."
                if ! eval "/bin/bash -c \"\$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)\" $exec_redirect"; then
                    echo "Failed to install Homebrew"
                    exit 1
                fi
            else
                echo "Please install Python3 and pip manually and rerun this script."
                exit 1
            fi
        fi
        echo "Installing Python3..."
        run_cmd brew install python3
    else
        echo "Unsupported OS"
        exit 1
    fi
fi

# Upgrade pip
echo "Upgrading pip..."
run_cmd python3 -m pip install --user --upgrade pip

# Install pyinfra
echo "Installing pyinfra..."
run_cmd python3 -m pip install --user pyinfra

# Get Python user bin path directly from Python
PYTHON_USER_BIN=$(python3 -c 'import site; print(site.USER_BASE + "/bin")')

# Add to PATH if not already there
if [[ ":$PATH:" != *":$PYTHON_USER_BIN:"* ]]; then
    export PATH="$PYTHON_USER_BIN:$PATH"
fi

# Verify pyinfra is accessible
if ! command -v pyinfra &> /dev/null; then
    echo "Failed to find pyinfra in PATH. Please ensure $PYTHON_USER_BIN is in your PATH"
    echo "You might need to run: export PATH=\"$PYTHON_USER_BIN:\$PATH\""
    exit 1
fi

# Run the main script
python3 sparx.py