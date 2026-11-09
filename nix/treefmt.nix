# Format policy for this repo. harbor-rs supplies the rustfmt module; we pin
# the edition (2024) and keep alejandra for Nix. max_width comes from
# rustfmt.toml, which rustfmt discovers from the project root.
{
  programs.rustfmt = {
    enable = true;
    edition = "2024";
  };
  programs.alejandra.enable = true;
}
