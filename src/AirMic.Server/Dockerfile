FROM mcr.microsoft.com/dotnet/sdk:10.0 AS build
WORKDIR /src

# Copy projects and restore dependencies
COPY ["src/AirMic.Server/AirMic.Server.csproj", "src/AirMic.Server/"]
COPY ["src/AirMic.Client/AirMic.Client.csproj", "src/AirMic.Client/"]
COPY ["shared/AirMic.Contracts/AirMic.Contracts.csproj", "shared/AirMic.Contracts/"]
RUN dotnet restore "src/AirMic.Server/AirMic.Server.csproj"

# Copy remaining source code and publish
COPY . .
WORKDIR "/src/src/AirMic.Server"
RUN dotnet publish "AirMic.Server.csproj" -c Release -o /app/publish

# Run stage
FROM mcr.microsoft.com/dotnet/aspnet:10.0 AS final
WORKDIR /app
COPY --from=build /app/publish .

EXPOSE 8443
ENV STREAM_SECRET="YourCustomSecretKeyHere"
ENV PORT=8443

ENTRYPOINT ["dotnet", "AirMic.Server.dll"]
