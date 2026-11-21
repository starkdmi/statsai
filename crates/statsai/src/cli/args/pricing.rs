use super::*;

#[derive(Debug, Args)]
pub(crate) struct PricingCommand {
    #[command(subcommand)]
    pub(crate) command: PricingSubcommand,
}

#[derive(Debug, Subcommand)]
pub(crate) enum PricingSubcommand {
    #[command(
        about = "Show the pricing files this binary and the store hold, and the refresh state"
    )]
    Status {
        #[arg(long, help = "Output as JSON")]
        json: bool,
    },
    #[command(
        about = "Fetch the pricing file from GitHub now, ignoring the daily schedule, and reprice the store with it"
    )]
    Refresh {
        /// The process `statsai scan` and `statsai sync` start: refresh only
        /// when due, print nothing, always succeed, and leave repricing to
        /// the next command that reads prices.
        #[arg(long, hide = true)]
        background: bool,
    },
    #[command(about = "Turn the daily pricing refresh on or off for this store")]
    AutoRefresh {
        #[arg(
            value_enum,
            help = "Whether StatsAI refreshes the pricing file for this store"
        )]
        state: OnOff,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum OnOff {
    On,
    Off,
}

impl OnOff {
    pub(crate) fn is_on(self) -> bool {
        matches!(self, Self::On)
    }
}
