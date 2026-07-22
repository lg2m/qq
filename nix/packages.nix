_: {
  perSystem =
    { pkgs, ... }:
    {
      packages.rust-toolchain = pkgs.qq-rust-toolchain;
    };
}
