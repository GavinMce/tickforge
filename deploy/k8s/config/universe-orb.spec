universe v1
# The names of the paper (Zarattini, Barbon and Aziz): a price over $5, average volume over a million shares and an ATR over fifty cents. The strategy reads the ATR and the volume
# of the first five minutes (and of the first quarter hour, for the fifteen minute candle) from the snapshot, so a snapshot without the minute history refuses to run it.
requires atr14 vol_first5 qvol_0930
static price >= 5; adv_shares >= 1000000; atr14 >= 0.50
