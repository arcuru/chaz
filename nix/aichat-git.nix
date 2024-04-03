{
  lib,
  stdenv,
  darwin,
  rustPlatform,
  fetchFromGitHub,
  pkg-config,
}:
rustPlatform.buildRustPackage rec {
  pname = "aichat";
  version = "84695b62c696efd29cbf1c5c891f1fbdcbfb11ae";

  src = fetchFromGitHub {
    owner = "sigoden";
    repo = "aichat";
    rev = "${version}";
    hash = "sha256-5U4v/W/oURR9XtdYRMXUm/pXWvfdh3SqPtpK94iw1Ac=";
  };

  cargoHash = "sha256-N/VPjlGv/E/ezX3hu9pea3dDDJoiJQtwAf+HyDemH+8=";

  nativeBuildInputs = [
    pkg-config
  ];

  buildInputs = lib.optionals stdenv.isDarwin [
    darwin.apple_sdk.frameworks.AppKit
    darwin.apple_sdk.frameworks.CoreFoundation
    darwin.apple_sdk.frameworks.Security
  ];

  meta = with lib; {
    description = "Use GPT-4(V), Gemini, LocalAI, Ollama and other LLMs in the terminal";
    homepage = "https://github.com/sigoden/aichat";
    license = licenses.mit;
    maintainers = with maintainers; [mwdomino];
    mainProgram = "aichat";
  };
}
