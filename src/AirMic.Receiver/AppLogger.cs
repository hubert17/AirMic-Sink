using System;
using Serilog;

namespace AirMic.Receiver
{
    public static class AppLogger
    {
        public static void Information(string message)
        {
            Console.WriteLine(message);
            Log.Information(message);
        }

        public static void Success(string message)
        {
            Console.ForegroundColor = ConsoleColor.Green;
            Console.WriteLine(message);
            Console.ResetColor();
            Log.Information(message);
        }

        public static void Warning(string message)
        {
            Console.ForegroundColor = ConsoleColor.Yellow;
            Console.WriteLine(message);
            Console.ResetColor();
            Log.Warning(message);
        }

        public static void Warning(Exception ex, string message)
        {
            Console.ForegroundColor = ConsoleColor.Yellow;
            Console.WriteLine(message);
            Console.ResetColor();
            Log.Warning(ex, message);
        }

        public static void Error(string message)
        {
            Console.ForegroundColor = ConsoleColor.Red;
            Console.WriteLine(message);
            Console.ResetColor();
            Log.Error(message);
        }

        public static void Error(Exception ex, string message)
        {
            Console.ForegroundColor = ConsoleColor.Red;
            Console.WriteLine(message);
            Console.ResetColor();
            Log.Error(ex, message);
        }

        public static void Test(string message)
        {
            Console.ForegroundColor = ConsoleColor.Magenta; // Purple/Magenta
            Console.WriteLine(message);
            Console.ResetColor();
            Log.Information(message);
        }
    }
}
